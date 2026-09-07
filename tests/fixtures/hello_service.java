// Long-running service fixture for runtime tests.
// Run in single-file source mode: `java hello_service.java <args...>` (JEP 330).
// The JVM's shutdown hook runs on console ctrl events.
public class HelloService {
    public static void main(String[] args) throws Exception {
        for (int i = 0; i < args.length; i++) {
            System.out.println("ARG" + i + "=" + args[i]);
        }
        System.out.println("READY");
        System.out.flush();
        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            System.out.println("CLEAN-SHUTDOWN");
            System.out.flush();
        }));
        int n = 0;
        while (true) {
            Thread.sleep(300);
            System.out.println("TICK " + (n++));
            System.out.flush();
        }
    }
}
