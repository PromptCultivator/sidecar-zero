package s4;

import java.io.ByteArrayInputStream;
import java.io.IOException;
import java.io.PrintWriter;
import java.io.StringWriter;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.MappedByteBuffer;
import java.nio.channels.Channels;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.List;
import org.apache.arrow.flatbuf.Message;
import org.apache.arrow.flatbuf.MessageHeader;
import org.apache.arrow.memory.ArrowBuf;
import org.apache.arrow.memory.BoundsChecking;
import org.apache.arrow.memory.BufferAllocator;
import org.apache.arrow.memory.ForeignAllocation;
import org.apache.arrow.memory.RootAllocator;
import org.apache.arrow.memory.util.MemoryUtil;
import org.apache.arrow.vector.BigIntVector;
import org.apache.arrow.vector.FieldVector;
import org.apache.arrow.vector.Float4Vector;
import org.apache.arrow.vector.Float8Vector;
import org.apache.arrow.vector.IntVector;
import org.apache.arrow.vector.NullCheckingForGet;
import org.apache.arrow.vector.UInt1Vector;
import org.apache.arrow.vector.VectorLoader;
import org.apache.arrow.vector.VectorSchemaRoot;
import org.apache.arrow.vector.complex.FixedSizeListVector;
import org.apache.arrow.vector.ipc.ArrowStreamReader;
import org.apache.arrow.vector.ipc.ReadChannel;
import org.apache.arrow.vector.ipc.message.ArrowRecordBatch;
import org.apache.arrow.vector.ipc.message.MessageSerializer;
import org.apache.arrow.vector.types.pojo.Field;
import org.apache.arrow.vector.types.pojo.Schema;
import org.apache.arrow.vector.util.ValueVectorUtility;

/**
 * Spike S4, Java side (Arrow Java: arrow-vector + arrow-memory-unsafe).
 *
 * <p>usage: Reader FRAMES_DIR -- reads every frame in manifest.tsv, prints one JSON line per (frame, mode).
 *
 * <p>Modes:
 *
 * <ul>
 *   <li>deserializeRecordBatch: the path docs/components/03-arrow-data-plane.md section 7.4 names. The
 *       region is a shared file mapping under /dev/shm wrapped as an ArrowBuf over a ForeignAllocation;
 *       the flatbuffer Message is taken from it in place and decoded with
 *       MessageSerializer.deserializeRecordBatch(Message, ArrowBuf), then loaded with VectorLoader
 *       against the registry's schema.
 *   <li>ArrowStreamReader: the stock stream reader over Schema message + frame bytes.
 * </ul>
 *
 * Both then run ValueVectorUtility.validateFull on the loaded root.
 */
public final class Reader {
  static final String LIB = "arrow-java 19.0.0 (arrow-vector, arrow-memory-unsafe)";

  // ---------------------------------------------------------------- checksum

  static long feed(long h, int b) {
    return (h ^ (b & 0xffL)) * 0x100000001b3L;
  }

  static long feedInt(long h, int v) {
    for (int i = 0; i < 4; i++) {
      h = feed(h, v >>> (8 * i));
    }
    return h;
  }

  static long feedLong(long h, long v) {
    for (int i = 0; i < 8; i++) {
      h = feed(h, (int) (v >>> (8 * i)));
    }
    return h;
  }

  static String columnChecksum(FieldVector v, int rows) {
    long h = 0xcbf29ce484222325L;
    for (int r = 0; r < rows; r++) {
      if (v.isNull(r)) {
        h = feed(h, 0);
        continue;
      }
      h = feed(h, 1);
      if (v instanceof FixedSizeListVector l) {
        int n = l.getListSize();
        Float4Vector child = (Float4Vector) l.getDataVector();
        for (int j = 0; j < n; j++) {
          h = feedInt(h, Float.floatToRawIntBits(child.get(r * n + j)));
        }
      } else if (v instanceof BigIntVector b) {
        h = feedLong(h, b.get(r));
      } else if (v instanceof Float8Vector f) {
        h = feedLong(h, Double.doubleToRawLongBits(f.get(r)));
      } else if (v instanceof IntVector i) {
        h = feedInt(h, i.get(r));
      } else if (v instanceof UInt1Vector u) {
        h = feed(h, u.get(r));
      } else {
        throw new IllegalStateException("unexpected vector " + v.getClass().getSimpleName());
      }
    }
    return String.format("%016x", h);
  }

  static long valuesAddress(FieldVector v) {
    if (v instanceof FixedSizeListVector l) {
      return l.getDataVector().getDataBuffer().memoryAddress();
    }
    return v.getDataBuffer().memoryAddress();
  }

  // ---------------------------------------------------------------- output

  static String js(String s) {
    StringBuilder o = new StringBuilder("\"");
    for (char c : s.toCharArray()) {
      switch (c) {
        case '"' -> o.append("\\\"");
        case '\\' -> o.append("\\\\");
        case '\n' -> o.append("\\n");
        default -> {
          if (c < 0x20) {
            o.append(String.format("\\u%04x", (int) c));
          } else {
            o.append(c);
          }
        }
      }
    }
    return o.append('"').toString();
  }

  static String describe(VectorSchemaRoot root, long base, long len, String detail) {
    int rows = root.getRowCount();
    List<String> fields = new ArrayList<>();
    for (Field f : root.getSchema().getFields()) {
      String t = f.getType().toString();
      if (!f.getChildren().isEmpty()) {
        Field c = f.getChildren().get(0);
        t += "<" + c.getName() + ": " + c.getType() + (c.isNullable() ? "" : " not null") + ">";
      }
      fields.add(f.getName() + ": " + t + (f.isNullable() ? "" : " not null"));
    }
    String vf = "ok";
    for (FieldVector v : root.getFieldVectors()) {
      try {
        try {
          ValueVectorUtility.validateFull(v);
        } catch (UnsupportedOperationException e) {
          // Arrow Java's full validation does not handle FixedSizeListVector at all (see
          // FslValidateProbe). Fall back to validate() on the list and validateFull() on its child.
          if (!(v instanceof FixedSizeListVector l)) {
            throw e;
          }
          ValueVectorUtility.validate(l);
          ValueVectorUtility.validateFull(l.getDataVector());
          vf = "ok (list: validate, child: validateFull; validateFull on FixedSizeListVector is unsupported)";
        }
      } catch (RuntimeException e) {
        vf = v.getName() + ": " + e.getClass().getSimpleName() + ": " + e.getMessage();
        break;
      }
    }
    List<String> cols = new ArrayList<>();
    boolean borrowed = true;
    for (FieldVector v : root.getFieldVectors()) {
      cols.add(
          "{\"name\":" + js(v.getName()) + ",\"nulls\":" + v.getNullCount() + ",\"fnv\":" + js(columnChecksum(v, rows)) + "}");
      long a = valuesAddress(v);
      borrowed &= a >= base && a < base + len;
    }
    return ",\"ok\":true,\"rows\":" + rows
        + ",\"schema\":" + js(String.join("; ", fields))
        + ",\"validate_full\":" + js(vf)
        + ",\"cols\":[" + String.join(",", cols) + "]"
        + (len > 0 ? ",\"borrowed\":" + borrowed : "")
        + ",\"detail\":" + js(detail) + "}";
  }

  interface Mode {
    String run() throws Exception;
  }

  /** A failure whose message is already complete (original exception plus what closing cost). */
  static final class Failure extends Exception {
    Failure(String message, Throwable cause) {
      super(message, cause);
    }
  }

  /** Closes a child allocator; returns the leak report if Arrow Java refuses to close it. */
  static String closeAllocator(BufferAllocator alloc) {
    try {
      alloc.close();
      return null;
    } catch (IllegalStateException e) {
      return e.getMessage().lines().findFirst().orElse("leak");
    }
  }

  static void report(String frameId, String mode, Mode m) {
    String head = "{\"reader\":\"java\",\"lib\":" + js(LIB) + ",\"mode\":" + js(mode) + ",\"frame\":" + js(frameId);
    String tail;
    try {
      tail = m.run();
    } catch (Throwable t) {
      StringWriter sw = new StringWriter();
      t.printStackTrace(new PrintWriter(sw));
      System.err.println("[" + frameId + " / " + mode + "] " + sw);
      String what = t instanceof Failure ? t.getMessage() : t.getClass().getName() + ": " + t.getMessage();
      tail = ",\"ok\":false,\"error\":" + js(what) + "}";
    }
    System.out.println(head + tail);
  }

  // ---------------------------------------------------------------- modes

  /** A shared mapping under /dev/shm holding the frame: page-aligned shared memory, like a slot. */
  static MappedByteBuffer mapRegion(byte[] frame) throws IOException {
    Path p = Files.createTempFile(Path.of("/dev/shm"), "s4-region-", ".bin");
    try (FileChannel ch = FileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
      MappedByteBuffer mbb = ch.map(FileChannel.MapMode.READ_WRITE, 0, Math.max(frame.length, 1));
      mbb.put(0, frame);
      return mbb;
    } finally {
      Files.delete(p);
    }
  }

  static Schema readSchema(byte[] schemaMsg) throws IOException {
    return MessageSerializer.deserializeSchema(
        new ReadChannel(Channels.newChannel(new ByteArrayInputStream(schemaMsg))));
  }

  static String modeDeserialize(BufferAllocator root, byte[] schemaMsg, byte[] frame) throws Exception {
    Schema schema = readSchema(schemaMsg);
    MappedByteBuffer mbb = mapRegion(frame);
    long addr = MemoryUtil.getByteBufferAddress(mbb);
    BufferAllocator alloc = root.newChildAllocator("slot", 0, Long.MAX_VALUE);
    ArrowBuf region =
        alloc.wrapForeignAllocation(
            new ForeignAllocation(frame.length, addr) {
              private final MappedByteBuffer keep = mbb; // the mapping outlives the ArrowBuf

              @Override
              protected void release0() {}
            });
    try {
      if (region.getInt(0) != 0xFFFFFFFF) {
        throw new IOException("no continuation marker");
      }
      int metaLen = region.getInt(4);
      if (metaLen < 4 || 8L + metaLen > frame.length) {
        throw new IOException("metadata_size runs past the region");
      }
      ByteBuffer meta = region.nioBuffer(8, metaLen).order(ByteOrder.LITTLE_ENDIAN);
      Message msg = Message.getRootAsMessage(meta);
      if (msg.headerType() != MessageHeader.RecordBatch) {
        throw new IOException("header type " + msg.headerType());
      }
      long bodyLen = msg.bodyLength();
      if (bodyLen < 0 || 8L + metaLen + bodyLen > frame.length) {
        throw new IOException("bodyLength runs past the region");
      }
      ArrowBuf body = region.slice(8L + metaLen, bodyLen);
      String detail = "metadata=" + metaLen + "B body=" + bodyLen + "B region@" + (addr % 4096) + "mod4096";
      // deserializeRecordBatch releases the body it is given once it has sliced it, and does not
      // release it when it throws; so hand it its own reference and take it back on failure.
      body.getReferenceManager().retain();
      ArrowRecordBatch arb;
      try {
        arb = MessageSerializer.deserializeRecordBatch(msg, body);
      } catch (Throwable t) {
        body.getReferenceManager().release();
        throw t;
      }
      try (arb;
          VectorSchemaRoot vsr = VectorSchemaRoot.create(schema, alloc)) {
        new VectorLoader(vsr).load(arb);
        return describe(vsr, addr, frame.length, detail);
      }
    } finally {
      region.close();
      String leak = closeAllocator(alloc);
      if (leak != null) {
        System.err.println("slot allocator: " + leak);
      }
    }
  }

  static String modeStream(BufferAllocator root, byte[] schemaMsg, byte[] frame) throws Exception {
    byte[] stream = new byte[schemaMsg.length + frame.length];
    System.arraycopy(schemaMsg, 0, stream, 0, schemaMsg.length);
    System.arraycopy(frame, 0, stream, schemaMsg.length, frame.length);
    BufferAllocator alloc = root.newChildAllocator("stream", 0, Long.MAX_VALUE);
    String out = null;
    Throwable failure = null;
    try (ArrowStreamReader rdr = new ArrowStreamReader(new ByteArrayInputStream(stream), alloc)) {
      VectorSchemaRoot vsr = rdr.getVectorSchemaRoot();
      if (!rdr.loadNextBatch()) {
        throw new IOException("stream ended before a batch");
      }
      String first = describe(vsr, 0, 0, "@@");
      String detail = rdr.loadNextBatch() ? "a second batch followed" : "stream ended cleanly after the frame";
      out = first.replace("\"@@\"", js(detail));
    } catch (Throwable t) {
      failure = t;
    }
    // Nothing of ours is still open here: whatever the allocator reports was left by the reader.
    String leak = closeAllocator(alloc);
    if (failure != null) {
      String what = failure.getClass().getName() + ": " + failure.getMessage();
      throw new Failure(leak == null ? what : what + " [closing the reader's allocator then failed: " + leak + "]", failure);
    }
    if (leak != null) {
      throw new Failure("read succeeded but the allocator reported: " + leak, null);
    }
    return out;
  }

  public static void main(String[] args) throws Exception {
    if (args.length != 1) {
      System.err.println("usage: Reader FRAMES_DIR");
      System.exit(2);
    }
    System.err.println(LIB + "; java " + Runtime.version()
        + "; BoundsChecking.BOUNDS_CHECKING_ENABLED=" + BoundsChecking.BOUNDS_CHECKING_ENABLED
        + "; NullCheckingForGet.NULL_CHECKING_ENABLED=" + NullCheckingForGet.NULL_CHECKING_ENABLED);
    Path dir = Path.of(args[0]);
    BufferAllocator root = new RootAllocator();
    for (String line : Files.readAllLines(dir.resolve("manifest.tsv"))) {
      String[] f = line.split("\t");
      byte[] schemaMsg = Files.readAllBytes(dir.resolve(f[5]));
      byte[] frame = Files.readAllBytes(dir.resolve(f[6]));
      report(f[0], "deserializeRecordBatch", () -> modeDeserialize(root, schemaMsg, frame));
      report(f[0], "ArrowStreamReader", () -> modeStream(root, schemaMsg, frame));
    }
    try {
      root.close();
    } catch (IllegalStateException e) {
      // Expected only if a negative-control frame made a reader leak; already reported per frame.
      System.err.println("root allocator: " + e.getMessage().lines().findFirst().orElse("leak"));
    }
  }

  private Reader() {}
}
