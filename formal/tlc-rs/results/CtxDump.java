import tlc2.tool.impl.FastTool;
import tla2sany.semantic.*;
import java.lang.reflect.*;
import java.util.*;

// Loads a spec + cfg the way TLC does, then prints the root module's SANY
// context: its size, the Hashtable's capacity, and every name in the order
// it was inserted (the context's Pair chain, reversed).
public class CtxDump {
    public static void main(String[] a) throws Exception {
        FastTool t = new FastTool(a[0], a[1]);
        ModuleNode root = t.getSpecProcessor().getRootModule();
        Context c = root.getContext();
        Field tf = Context.class.getDeclaredField("table"); tf.setAccessible(true);
        Hashtable<?, ?> h = (Hashtable<?, ?>) tf.get(c);
        Field cap = Hashtable.class.getDeclaredField("table"); cap.setAccessible(true);
        System.out.println("count " + h.size() + " capacity " + Array.getLength(cap.get(h)));
        Field lp = Context.class.getDeclaredField("lastPair"); lp.setAccessible(true);
        Object p = lp.get(c);
        List<String> names = new ArrayList<>();
        while (p != null) {
            Field info = p.getClass().getDeclaredField("info"); info.setAccessible(true);
            Field link = p.getClass().getDeclaredField("link"); link.setAccessible(true);
            SymbolNode s = (SymbolNode) info.get(p);
            names.add(s.getName() + (s.isLocal() ? " (local)" : "") + " " + s.getClass().getSimpleName());
            p = link.get(p);
        }
        Collections.reverse(names);
        for (String n : names) System.out.println("  " + n);
    }
}
