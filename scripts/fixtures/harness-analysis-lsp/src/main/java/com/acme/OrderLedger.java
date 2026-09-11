package com.acme;
import java.util.ArrayList;
import java.util.List;
public class OrderLedger {
    private final String prefix = "order";
    private final String label = prefix + "-ledger";
    private final List<String> receipts = new ArrayList<>();
    private int pending = 0;
    private int settled = 0;
    public void deposit(int amount) {
        int accepted = Math.max(0, amount);
        pending += accepted;
        receipts.add(label + ":" + accepted);
    }
    public void settle() {
        settled += pending;
        pending = 0;
    }
    public int balance() { return settled + pending; }
    public List<String> receipts() { return receipts; }
    public String summary(int tax) {
        int total = balance();
        int due = total + tax;
        String text = label + ":" + due;
        receipts.add(text);
        return text;
    }
}
