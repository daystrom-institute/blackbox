public class Hello {

    public String greet() {
        return "Hello, World!";
    }

    public static void main(String[] args) {
        Hello h = new Hello();
        System.out.println(h.greet());
        String msg = h.greet();
        System.out.println(msg);
    }
}
