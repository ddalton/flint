import tlc2.tool.impl.FastTool;
import util.UniqueString;
import java.io.*;

// Loads a spec + cfg the way TLC does, then prints TLC's intern token for
// each name on stdin (name TAB token).
public class TokDump {
    public static void main(String[] a) throws Exception {
        java.util.List<String> names = new java.util.ArrayList<>();
        BufferedReader r = new BufferedReader(new InputStreamReader(System.in));
        String l;
        while ((l = r.readLine()) != null) names.add(l.split("\t")[0]);
        new FastTool(a[0], a[1]);
        for (String n : names) System.out.println(n + "\t" + UniqueString.uniqueStringOf(n).getTok());
    }
}
