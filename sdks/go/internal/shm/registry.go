package shm

import (
	"encoding/binary"
	"errors"
	"fmt"
	"unicode/utf8"

	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/abi"
)

// Column is one column as the fingerprint and the codecs see it.
type Column struct {
	Name string
	Elem uint8
	// ListSize is 1 for a scalar, N for FixedSizeList<T>[N], the byte width for FIXED_BINARY.
	ListSize uint32
	Nullable bool
}

// ElemWidth is the bytes per element of a physical type; 0 for an unknown code.
func ElemWidth(elem uint8) uint32 {
	switch uint32(elem) {
	case abi.ElemTypeI8, abi.ElemTypeU8, abi.ElemTypeFixedBinary:
		return 1
	case abi.ElemTypeI16, abi.ElemTypeU16, abi.ElemTypeF16:
		return 2
	case abi.ElemTypeI32, abi.ElemTypeU32, abi.ElemTypeF32:
		return 4
	case abi.ElemTypeI64, abi.ElemTypeU64, abi.ElemTypeF64:
		return 8
	}
	return 0
}

// RowBytes is the bytes of values per row, or 0 if the type is unknown or it overflows.
func (c Column) RowBytes() uint32 {
	w := uint64(ElemWidth(c.Elem)) * uint64(c.ListSize)
	if w > 0xFFFFFFFF {
		return 0
	}
	return uint32(w)
}

// Schema is a model's request and response columns.
type Schema struct {
	Request, Response []Column
}

// Fingerprint is arrow-data-plane §9: xxHash3-64, seed 0, over the logical schema. Golden
// vectors: abi/golden/fingerprints.txt.
func Fingerprint(s Schema) uint64 {
	b := []byte("SCZSCHEMA\x01")
	for _, cols := range [2][]Column{s.Request, s.Response} {
		b = binary.LittleEndian.AppendUint16(b, uint16(len(cols)))
		for _, c := range cols {
			b = binary.LittleEndian.AppendUint16(b, uint16(len(c.Name)))
			b = append(b, c.Name...)
			b = append(b, c.Elem)
			b = binary.LittleEndian.AppendUint32(b, c.ListSize)
			nb := byte(0)
			if c.Nullable {
				nb = 1
			}
			b = append(b, nb)
		}
	}
	return XXH3(b)
}

// ColumnLoc is where one column's values sit in a region laid out by a template.
type ColumnLoc struct {
	Column    Column
	ValuesOff uint32
	RowBytes  uint32
}

// Patch is a length field in the prefix, and how it follows the row count.
type Patch struct {
	PrefixOff uint32
	Kind      uint8
	Scale     uint32
}

// Value is the field's value for rows rows.
func (p Patch) Value(rows uint32) uint64 {
	if uint32(p.Kind) == abi.PatchKindBitmapBytes {
		return (uint64(rows) + 7) / 8
	}
	return uint64(rows) * uint64(p.Scale)
}

// Template is one slot class's frame for one direction (arrow-data-plane §4).
type Template struct {
	ID           uint16
	Direction    uint8
	Class        uint16
	MaxRows      uint32
	Prefix       []byte
	BodyOff      uint32
	FrameCap     uint32
	RowsFieldOff uint32
	Columns      []ColumnLoc
	Patches      []Patch
}

// Stamp is (registry_epoch low 16 bits) << 16 | template_id: what a region holds once stamped.
func (t *Template) Stamp(epoch uint32) uint32 { return (epoch&0xFFFF)<<16 | uint32(t.ID) }

// Registry is a parsed registry.
type Registry struct {
	Schema    Schema
	InSchema  []byte
	OutSchema []byte
	Templates []Template
	req, resp [abi.MaxSlabClasses]*Template
}

// Fingerprint is the registry's schema fingerprint; 0 for one without templates.
func (r *Registry) Fingerprint() uint64 {
	if r == nil || len(r.Templates) == 0 {
		return 0
	}
	return Fingerprint(r.Schema)
}

// Request and Response are a class's templates, or nil.
func (r *Registry) Request(class int) *Template  { return r.req[class] }
func (r *Registry) Response(class int) *Template { return r.resp[class] }

// ClassFor is the smallest class whose request template holds rows (arrow-data-plane §4),
// or -1.
func (r *Registry) ClassFor(rows uint32) int {
	best := -1
	var bestRows uint32
	for c := range r.req {
		t := r.req[c]
		if t != nil && t.MaxRows >= rows && (best < 0 || t.MaxRows < bestRows) {
			best, bestRows = c, t.MaxRows
		}
	}
	return best
}

// Caps are a slot class's two region capacities.
type Caps struct{ Req, Resp uint32 }

type reader []byte

var errPast = errors.New("a read past the registry")

func (r reader) get(at, n int) ([]byte, error) {
	if at < 0 || n < 0 || at+n < at || at+n > len(r) {
		return nil, errPast
	}
	return r[at : at+n], nil
}
func (r reader) u8(at int) (uint8, error) {
	b, err := r.get(at, 1)
	if err != nil {
		return 0, err
	}
	return b[0], nil
}
func (r reader) u16(at int) (uint16, error) {
	b, err := r.get(at, 2)
	if err != nil {
		return 0, err
	}
	return binary.LittleEndian.Uint16(b), nil
}
func (r reader) u32(at int) (uint32, error) {
	b, err := r.get(at, 4)
	if err != nil {
		return 0, err
	}
	return binary.LittleEndian.Uint32(b), nil
}
func (r reader) u64(at int) (uint64, error) {
	b, err := r.get(at, 8)
	if err != nil {
		return 0, err
	}
	return binary.LittleEndian.Uint64(b), nil
}

func (r reader) array(off uint32, count, stride, minSize, align int) (int, error) {
	a := align
	if a > 4 {
		a = 4
	}
	if stride < minSize || stride%a != 0 {
		return 0, fmt.Errorf("stride %d below %d", stride, minSize)
	}
	if int(off)%align != 0 {
		return 0, fmt.Errorf("array at %#x is misaligned", off)
	}
	end := uint64(off) + uint64(count)*uint64(stride)
	if end > uint64(len(r)) {
		return 0, fmt.Errorf("%d entries at %#x run past the registry", count, off)
	}
	return int(off), nil
}

// ParseRegistry parses a registry copied out of shared memory, checking everything the fast
// path relies on (arrow-data-plane §8, "fast path has nothing to trust"), exactly as the Rust
// reference's registry::parse does. (nil, nil) is an empty registry: opaque bytes.
func ParseRegistry(bytes []byte, caps []Caps) (*Registry, error) {
	if len(bytes) == 0 {
		return nil, nil
	}
	r := reader(bytes)
	must := func(err error) bool { return err != nil }
	magic, err := r.u64(abi.RegistryHeaderMagic)
	if must(err) {
		return nil, err
	}
	if magic != abi.RegistryMagic {
		return nil, errors.New("bad magic")
	}
	n32 := func(at int) uint32 { v, e := r.u32(at); err = errors.Join(err, e); return v }
	n16 := func(at int) uint16 { v, e := r.u16(at); err = errors.Join(err, e); return v }
	n8 := func(at int) uint8 { v, e := r.u8(at); err = errors.Join(err, e); return v }
	length := int(n32(abi.RegistryHeaderRegistryLen))
	if length != len(bytes) || length > abi.CtlRegistryLen {
		return nil, fmt.Errorf("registry_len %d for %d bytes", length, len(bytes))
	}
	nt := int(n16(abi.RegistryHeaderTemplateCount))
	ts, cs, ps := int(n16(abi.RegistryHeaderTemplateStride)), int(n16(abi.RegistryHeaderColumnStride)), int(n16(abi.RegistryHeaderPatchStride))
	nc, np := int(n32(abi.RegistryHeaderColumnTotal)), int(n32(abi.RegistryHeaderPatchTotal))
	if err != nil {
		return nil, err
	}
	if nt == 0 || nt > abi.MaxTemplates || nc > nt*abi.MaxColumns || np > nt*abi.MaxPatches {
		return nil, fmt.Errorf("%d templates, %d columns, %d patches", nt, nc, np)
	}
	toff, err := r.array(n32(abi.RegistryHeaderTemplatesOff), nt, ts, abi.TemplateDescSize, 8)
	if err != nil {
		return nil, err
	}
	coff, err := r.array(n32(abi.RegistryHeaderColumnsOff), nc, cs, abi.ColumnDescSize, 4)
	if err != nil {
		return nil, err
	}
	poff, err := r.array(n32(abi.RegistryHeaderPatchesOff), np, ps, abi.PatchDescSize, 4)
	if err != nil {
		return nil, err
	}
	names, err := r.get(int(n32(abi.RegistryHeaderNamesOff)), int(n32(abi.RegistryHeaderNamesLen)))
	if err != nil {
		return nil, err
	}
	inSchema, err := r.get(int(n32(abi.RegistryHeaderInSchemaOff)), int(n32(abi.RegistryHeaderInSchemaLen)))
	if err != nil {
		return nil, err
	}
	outSchema, err := r.get(int(n32(abi.RegistryHeaderOutSchemaOff)), int(n32(abi.RegistryHeaderOutSchemaLen)))
	if err != nil {
		return nil, err
	}
	reg := &Registry{InSchema: append([]byte(nil), inSchema...), OutSchema: append([]byte(nil), outSchema...)}
	for k := 0; k < nt; k++ {
		at := toff + k*ts
		id := n16(at + abi.TemplateDescTemplateId)
		dir := n8(at + abi.TemplateDescDirection)
		class := n16(at + abi.TemplateDescSlabClass)
		if err != nil {
			return nil, err
		}
		what := fmt.Sprintf("template %d", id)
		if id == 0 {
			return nil, fmt.Errorf("%s: id 0 or repeated", what)
		}
		for i := range reg.Templates {
			if reg.Templates[i].ID == id {
				return nil, fmt.Errorf("%s: id 0 or repeated", what)
			}
		}
		if int(class) >= len(caps) {
			return nil, fmt.Errorf("%s: no slot class %d", what, class)
		}
		var cp uint32
		switch uint32(dir) {
		case abi.DirectionRequest:
			cp = caps[class].Req
		case abi.DirectionResponse:
			cp = caps[class].Resp
		default:
			return nil, fmt.Errorf("%s: direction %d", what, dir)
		}
		for i := range reg.Templates {
			if reg.Templates[i].Class == class && reg.Templates[i].Direction == dir {
				return nil, fmt.Errorf("%s: a second template for class %d", what, class)
			}
		}
		maxRows := n32(at + abi.TemplateDescMaxRows)
		prefixOff, prefixLen := n32(at+abi.TemplateDescPrefixOff), n32(at+abi.TemplateDescPrefixLen)
		bodyOff := n32(at + abi.TemplateDescBodyOff)
		frameCap := n32(at + abi.TemplateDescFrameCap)
		rowsFieldOff := n32(at + abi.TemplateDescRowsFieldOff)
		c0, ncol := int(n32(at+abi.TemplateDescFirstColumn)), int(n16(at+abi.TemplateDescColumnCount))
		p0, npat := int(n32(at+abi.TemplateDescFirstPatch)), int(n16(at+abi.TemplateDescPatchCount))
		if err != nil {
			return nil, err
		}
		prefix, e := r.get(int(prefixOff), int(prefixLen))
		if e != nil {
			return nil, e
		}
		if maxRows == 0 || len(prefix) < 8 || int(bodyOff) != len(prefix) || bodyOff%8 != 0 || frameCap < bodyOff || frameCap > cp {
			return nil, fmt.Errorf("%s: max_rows %d, prefix %d bytes, body at %d, frame %d in a %d byte region", what, maxRows, len(prefix), bodyOff, frameCap, cp)
		}
		inPrefix := func(off uint32) bool { return off%8 == 0 && int(off)+8 <= len(prefix) }
		if !inPrefix(rowsFieldOff) {
			return nil, fmt.Errorf("%s: row count field at %d outside the prefix", what, rowsFieldOff)
		}
		if c0+ncol < c0 || c0+ncol > nc || p0+npat < p0 || p0+npat > np || ncol > abi.MaxColumns || npat > abi.MaxPatches {
			return nil, fmt.Errorf("%s: column or patch range out of bounds", what)
		}
		t := Template{ID: id, Direction: dir, Class: class, MaxRows: maxRows, Prefix: append([]byte(nil), prefix...), BodyOff: bodyOff, FrameCap: frameCap, RowsFieldOff: rowsFieldOff}
		for i := c0; i < c0+ncol; i++ {
			at := coff + i*cs
			nameOff, nameLen := int(n32(at+abi.ColumnDescNameOff)), int(n16(at+abi.ColumnDescNameLen))
			if err != nil {
				return nil, err
			}
			if nameOff+nameLen > len(names) || nameOff+nameLen < nameOff {
				return nil, fmt.Errorf("%s: a column name outside the names", what)
			}
			name := names[nameOff : nameOff+nameLen]
			if !utf8.Valid(name) {
				return nil, fmt.Errorf("%s: a column name that is not UTF-8", what)
			}
			col := Column{Name: string(name), Elem: n8(at + abi.ColumnDescElemType), ListSize: n32(at + abi.ColumnDescListSize), Nullable: n8(at+abi.ColumnDescNullable) != 0}
			valuesOff, rowBytes := n32(at+abi.ColumnDescValuesOff), n32(at+abi.ColumnDescRowBytes)
			validity, nullCount := n32(at+abi.ColumnDescValidityOff), n32(at+abi.ColumnDescNullCountOff)
			if err != nil {
				return nil, err
			}
			if col.Nullable || validity != 0 || nullCount != 0 {
				return nil, fmt.Errorf("%s: column %q is nullable, which version 1 does not carry", what, col.Name)
			}
			if col.ListSize == 0 || col.RowBytes() != rowBytes || rowBytes == 0 {
				return nil, fmt.Errorf("%s: column %q has %d bytes per row for type %d × %d", what, col.Name, rowBytes, col.Elem, col.ListSize)
			}
			end := uint64(valuesOff) + uint64(maxRows)*uint64(rowBytes)
			if valuesOff < bodyOff || valuesOff%8 != 0 || end > uint64(frameCap) {
				return nil, fmt.Errorf("%s: column %q values at %d for %d rows outside the frame", what, col.Name, valuesOff, maxRows)
			}
			t.Columns = append(t.Columns, ColumnLoc{Column: col, ValuesOff: valuesOff, RowBytes: rowBytes})
		}
		hasRows := false
		for i := p0; i < p0+npat; i++ {
			at := poff + i*ps
			p := Patch{PrefixOff: n32(at + abi.PatchDescPrefixOff), Scale: n32(at + abi.PatchDescScale), Kind: n8(at + abi.PatchDescKind)}
			if err != nil {
				return nil, err
			}
			okKind := (uint32(p.Kind) == abi.PatchKindRowsScaled && p.Scale > 0) || uint32(p.Kind) == abi.PatchKindBitmapBytes
			if !okKind || !inPrefix(p.PrefixOff) || p.PrefixOff < 8 {
				return nil, fmt.Errorf("%s: a patch at %d of kind %d outside the prefix", what, p.PrefixOff, p.Kind)
			}
			if p.PrefixOff == rowsFieldOff && uint32(p.Kind) == abi.PatchKindRowsScaled && p.Scale == 1 {
				hasRows = true
			}
			t.Patches = append(t.Patches, p)
		}
		if !hasRows {
			return nil, fmt.Errorf("%s: no patch rewrites the row count", what)
		}
		reg.Templates = append(reg.Templates, t)
	}
	// Every class has both templates or neither, with the same row capacity, and every
	// template of a direction carries the same columns.
	for i := range reg.Templates {
		t := &reg.Templates[i]
		other := uint8(abi.DirectionRequest)
		if uint32(t.Direction) == abi.DirectionRequest {
			other = abi.DirectionResponse
		}
		var u *Template
		for j := range reg.Templates {
			if reg.Templates[j].Class == t.Class && reg.Templates[j].Direction == other {
				u = &reg.Templates[j]
			}
		}
		if u == nil || u.MaxRows != t.MaxRows {
			return nil, fmt.Errorf("template %d: class %d lacks a matching template of the other direction", t.ID, t.Class)
		}
		cols := make([]Column, len(t.Columns))
		for k, c := range t.Columns {
			cols[k] = c.Column
		}
		slot := &reg.Schema.Request
		if uint32(t.Direction) == abi.DirectionResponse {
			slot = &reg.Schema.Response
		}
		if len(*slot) == 0 {
			*slot = cols
		} else if !sameColumns(*slot, cols) {
			return nil, fmt.Errorf("template %d: columns differ from another template of its direction", t.ID)
		}
	}
	if len(reg.Schema.Request) == 0 || len(reg.Schema.Response) == 0 {
		return nil, errors.New("a direction without columns")
	}
	for i := range reg.Templates {
		t := &reg.Templates[i]
		if int(t.Class) >= abi.MaxSlabClasses {
			return nil, fmt.Errorf("template %d: class %d", t.ID, t.Class)
		}
		if uint32(t.Direction) == abi.DirectionRequest {
			reg.req[t.Class] = t
		} else {
			reg.resp[t.Class] = t
		}
	}
	return reg, nil
}

func sameColumns(a, b []Column) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

// Prepare makes the request region of s a frame of t carrying rows rows (arrow-data-plane
// §7.1): the prefix copied in if the region's stamp is not want, then the length fields
// rewritten if the row count differs from the one the frame last held. The caller holds the
// slot CLAIMED.
func Prepare(data abi.Seg, s Slot, t *Template, want uint32, rows uint32) {
	if data.SlotRecordReqStamp(s.Rec) != want {
		data.CopyIn(s.Data, t.Prefix)
		data.SetSlotRecordReqStamp(s.Rec, want)
	}
	if data.Word64(s.Data+uintptr(t.RowsFieldOff)) != uint64(rows) {
		for _, p := range t.Patches {
			data.SetWord64(s.Data+uintptr(p.PrefixOff), p.Value(rows))
		}
	}
}

// Rows is the row count a frame of t at region holds, read once.
func Rows(data abi.Seg, region uintptr, t *Template) uint64 {
	return data.Word64(region + uintptr(t.RowsFieldOff))
}
