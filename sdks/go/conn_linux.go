//go:build linux

package sidecarzero

import (
	"encoding/binary"
	"errors"
	"net"
	"os"
	"syscall"
	"time"

	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/abi"
	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/shm"
)

// SDKVersion is what HELLO announces: major << 16 | minor << 8 | patch.
const SDKVersion = 0x00_01_00

const (
	backoffMin = 10 * time.Millisecond
	backoffMax = time.Second

	fGetSeals       = 1034
	sealSeal        = 0x1
	sealShrink      = 0x2
	sealGrow        = 0x4
	sealFutureWrite = 0x10
	ctlSeals        = sealGrow | sealShrink | sealFutureWrite | sealSeal
	dataSeals       = sealGrow | sealShrink | sealSeal
)

var (
	errBusy  = errors.New("busy")
	errAbi   = errors.New("abi mismatch")
	errOther = errors.New("handshake failed")
)

func jitter(d time.Duration) time.Duration {
	half := uint64(d / 2)
	return time.Duration(half + mix(uint64(time.Now().UnixNano()))%max(half, 1))
}

// background is the connection goroutine (sdk-contract §6): connect, bind, hold the session,
// rebind when the registry changes, retire, reconnect.
func (k *core) background() {
	defer close(k.done)
	backoff := backoffMin
	var busySince time.Time
	warned := false
	for !k.stop.Load() {
		k.setState(Connecting)
		a, err := k.connectOnce()
		switch err {
		case nil:
			busySince, backoff, warned = time.Time{}, backoffMin, false
			k.rebind.Store(false) // a request left over from an earlier session
			snap, _ := k.publish(a)
			k.sessions.Add(1)
			k.hold(a, snap)
			// Retirement starts now, before any reconnect (sdk-contract §6).
			k.setState(Disconnected)
			if !k.retire(k.current.Swap(nil)) {
				k.closeErr = errStillInUse
			}
		case errBusy:
			// Expected after a fast restart, and while the supervisor resets a session it asked
			// the client to leave: every 20 ms for the first second, then the ordinary backoff.
			k.setState(Disconnected)
			if busySince.IsZero() {
				busySince = time.Now()
			}
			if time.Since(busySince) < time.Second {
				k.nap(20 * time.Millisecond)
			} else {
				k.nap(jitter(backoff))
				backoff = min(2*backoff, backoffMax)
			}
		case errAbi:
			k.setState(Rejected)
			if !warned {
				k.opts.logf("sidecarzero: the sidecar at %s does not speak this SDK's ABI (%d.%d); serving fallbacks, retrying every 30 s", k.endpoint, abi.AbiMajor, abi.AbiMinor)
				warned = true
			}
			k.nap(30 * time.Second)
		default:
			k.setState(Disconnected)
			k.nap(jitter(backoff))
			backoff = min(2*backoff, backoffMax)
		}
	}
	k.setState(Disconnected)
}

// hold keeps the session until the supervisor's end closes or the client stops, rebinding
// when a request goroutine asks and re-checking a rejected schema once a second. The socket
// is idle in normal operation; end of stream is the only thing it carries. The goroutine
// waits in the runtime's poller, holding no OS thread.
func (k *core) hold(a *arena, snap *snapshot) {
	conn := a.conn.(*net.UnixConn)
	buf := make([]byte, abi.MsgLen)
	lastCheck := time.Now()
	for !k.stop.Load() {
		if !k.rebind.Load() && snap.mode == modeMismatch && time.Since(lastCheck) >= time.Second {
			lastCheck = time.Now()
			if e, ok := shm.ModelEpoch(a.ctl, 1000); ok && e != snap.epoch {
				k.rebind.Store(true)
			}
		}
		if k.rebind.Swap(false) {
			var old *snapshot
			snap, old = k.publish(a)
			if !k.retire(old) {
				k.closeErr = errStillInUse
			}
		}
		_ = conn.SetReadDeadline(time.Now().Add(5 * time.Millisecond))
		n, err := conn.Read(buf)
		if err != nil {
			var ne net.Error
			if errors.As(err, &ne) && ne.Timeout() {
				continue
			}
			return // end of stream, or reset: the session is over
		}
		if n == 0 {
			return
		}
		// A message: nothing is defined after READY yet; dropped.
	}
}

func fcntl(fd, cmd, arg int) (int, error) {
	r, _, e := syscall.Syscall(syscall.SYS_FCNTL, uintptr(fd), uintptr(cmd), uintptr(arg))
	if e != 0 {
		return -1, e
	}
	return int(r), nil
}

// connectOnce is the client side of the handshake (sdk-contract §6).
func (k *core) connectOnce() (_ *arena, err error) {
	timeout := k.opts.connectTimeout
	d := net.Dialer{Timeout: timeout}
	// unixpacket is SOCK_SEQPACKET; the standard library makes every socket close-on-exec.
	c, err := d.Dial("unixpacket", k.endpoint)
	if err != nil {
		return nil, errOther
	}
	conn := c.(*net.UnixConn)
	var a *arena
	defer func() {
		if a == nil {
			conn.Close()
		}
	}()
	_ = conn.SetDeadline(time.Now().Add(timeout))
	var hello [abi.MsgLen]byte
	le := binary.LittleEndian
	le.PutUint32(hello[abi.MsgHelloMagic:], abi.MsgMagic)
	le.PutUint32(hello[abi.MsgHelloKind:], abi.MsgKindHello)
	le.PutUint16(hello[abi.MsgHelloAbiMajor:], abi.AbiMajor)
	le.PutUint16(hello[abi.MsgHelloAbiMinorMin:], abi.AbiMinorMin)
	le.PutUint16(hello[abi.MsgHelloAbiMinorMax:], abi.AbiMinor)
	le.PutUint16(hello[abi.MsgHelloSdkLang:], abi.SdkLangGo)
	le.PutUint32(hello[abi.MsgHelloSdkVersion:], SDKVersion)
	le.PutUint32(hello[abi.MsgHelloPid:], uint32(os.Getpid()))
	if n, err := conn.Write(hello[:]); err != nil || n != len(hello) {
		return nil, errOther
	}
	// WELCOME, with the control and data memfds by SCM_RIGHTS. The standard library asks
	// recvmsg for MSG_CMSG_CLOEXEC on Linux; the flag is checked below all the same.
	var msg [abi.MsgLen]byte
	oob := make([]byte, syscall.CmsgSpace(4*4))
	n, oobn, flags, _, err := conn.ReadMsgUnix(msg[:], oob)
	// Take every descriptor first, so that none leaks whatever happens next.
	var fds []int
	if oobn > 0 {
		if cms, perr := syscall.ParseSocketControlMessage(oob[:oobn]); perr == nil {
			for _, cm := range cms {
				if rights, rerr := syscall.ParseUnixRights(&cm); rerr == nil {
					fds = append(fds, rights...)
				}
			}
		}
	}
	defer func() {
		for _, fd := range fds {
			syscall.Close(fd)
		}
	}()
	if err != nil || n == 0 || flags&(syscall.MSG_TRUNC|syscall.MSG_CTRUNC) != 0 || n != abi.MsgLen {
		return nil, errOther
	}
	if le.Uint32(msg[abi.MsgWelcomeMagic:]) != abi.MsgMagic || le.Uint32(msg[abi.MsgWelcomeKind:]) != abi.MsgKindWelcome {
		return nil, errOther
	}
	switch le.Uint32(msg[abi.MsgWelcomeStatus:]) {
	case abi.WelcomeStatusOk:
	case abi.WelcomeStatusBusy:
		return nil, errBusy
	case abi.WelcomeStatusAbiMismatch:
		return nil, errAbi
	default:
		return nil, errOther
	}
	if len(fds) != 2 {
		return nil, errOther
	}
	for _, fd := range fds {
		fl, err := fcntl(fd, syscall.F_GETFD, 0)
		if err != nil {
			return nil, errOther
		}
		if fl&syscall.FD_CLOEXEC == 0 {
			if _, err := fcntl(fd, syscall.F_SETFD, fl|syscall.FD_CLOEXEC); err != nil {
				return nil, errOther
			}
		}
	}
	ctlFd, dataFd := fds[0], fds[1]
	wantCtl := uint64(le.Uint32(msg[abi.MsgWelcomeCtlLen:]))
	wantData := le.Uint64(msg[abi.MsgWelcomeDataLen:])
	arenaID := le.Uint64(msg[abi.MsgWelcomeArenaId:])
	sessionID := le.Uint64(msg[abi.MsgWelcomeSessionId:])
	if s, err := fcntl(ctlFd, fGetSeals, 0); err != nil || s&ctlSeals != ctlSeals {
		return nil, errOther
	}
	if s, err := fcntl(dataFd, fGetSeals, 0); err != nil || s&dataSeals != dataSeals {
		return nil, errOther
	}
	ctlLen, err1 := syscall.Seek(ctlFd, 0, 2)
	dataLen, err2 := syscall.Seek(dataFd, 0, 2)
	if err1 != nil || err2 != nil || ctlLen < abi.CtlHeaderLen || uint64(ctlLen) != wantCtl || uint64(dataLen) != wantData || dataLen <= 0 {
		return nil, errOther
	}
	ctlMap, err := abi.Map(ctlFd, int(ctlLen), false)
	if err != nil {
		return nil, errOther
	}
	ok := false
	defer func() {
		if !ok {
			ctlMap.Unmap()
		}
	}()
	dir, err := shm.ReadDirectory(ctlMap.Seg(), uint64(dataLen))
	if err != nil || dir.ArenaID != arenaID {
		return nil, errOther
	}
	dataMap, err := abi.Map(dataFd, int(dataLen), true)
	if err != nil {
		return nil, errOther
	}
	data := dataMap.Seg()
	if data.StatusPageMagic(dir.StatusOff) != abi.DataMagic || data.StatusPageArenaId(dir.StatusOff) != dir.ArenaID {
		dataMap.Unmap()
		return nil, errOther
	}
	// Page tables now, on this goroutine, so request goroutines never take a first-touch
	// fault on a slot. The memory charge stays with the sidecar.
	ctlMap.Populate(false)
	dataMap.Populate(true)
	var ready [abi.MsgLen]byte
	le.PutUint32(ready[abi.MsgReadyMagic:], abi.MsgMagic)
	le.PutUint32(ready[abi.MsgReadyKind:], abi.MsgKindReady)
	le.PutUint64(ready[abi.MsgReadyArenaId:], arenaID)
	if n, err := conn.Write(ready[:]); err != nil || n != len(ready) {
		dataMap.Unmap()
		return nil, errOther
	}
	_ = conn.SetDeadline(time.Time{})
	ok = true
	a = &arena{ctl: ctlMap.Seg(), data: data, ctlMap: ctlMap, dataMap: dataMap, dir: dir, conn: conn, sessionID: sessionID}
	return a, nil
}
