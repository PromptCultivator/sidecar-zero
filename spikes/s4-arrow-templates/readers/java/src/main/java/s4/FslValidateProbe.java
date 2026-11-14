package s4;

import java.util.List;
import org.apache.arrow.memory.BufferAllocator;
import org.apache.arrow.memory.RootAllocator;
import org.apache.arrow.vector.Float4Vector;
import org.apache.arrow.vector.complex.FixedSizeListVector;
import org.apache.arrow.vector.types.FloatingPointPrecision;
import org.apache.arrow.vector.types.pojo.ArrowType;
import org.apache.arrow.vector.types.pojo.Field;
import org.apache.arrow.vector.types.pojo.FieldType;
import org.apache.arrow.vector.util.ValueVectorUtility;

/**
 * Spike S4 side-check: does Arrow Java's full validation work on a FixedSizeListVector that Arrow
 * Java built itself (no IPC, no template)? It does not, which is why Reader falls back for that type.
 */
public final class FslValidateProbe {
  public static void main(String[] a) throws Exception {
    try (BufferAllocator alloc = new RootAllocator()) {
      // A FixedSizeListVector built entirely by Arrow Java itself: no IPC, no template.
      Field child = new Field("item", new FieldType(false, new ArrowType.FloatingPoint(FloatingPointPrecision.SINGLE), null), null);
      Field f = new Field("features", new FieldType(false, new ArrowType.FixedSizeList(4), null), List.of(child));
      try (FixedSizeListVector v = (FixedSizeListVector) f.createVector(alloc)) {
        v.allocateNew();
        Float4Vector c = (Float4Vector) v.getDataVector();
        c.allocateNew(8);
        for (int i = 0; i < 8; i++) c.set(i, i);
        v.setNotNull(0); v.setNotNull(1);
        c.setValueCount(8);
        v.setValueCount(2);
        for (String which : new String[] {"validate", "validateFull"}) {
          try {
            if (which.equals("validate")) ValueVectorUtility.validate(v); else ValueVectorUtility.validateFull(v);
            System.out.println(which + ": ok");
          } catch (Throwable t) {
            System.out.println(which + ": " + t);
            for (int i = 0; i < Math.min(8, t.getStackTrace().length); i++) System.out.println("    at " + t.getStackTrace()[i]);
          }
        }
      }
    }
  }
}
