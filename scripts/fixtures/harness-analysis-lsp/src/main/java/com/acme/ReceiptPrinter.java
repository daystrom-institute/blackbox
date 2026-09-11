package com.acme;
public class ReceiptPrinter {
    public String print(OrderLedger ledger) {
        ledger.deposit(10);
        return ledger.summary(2) + ledger.balance();
    }
}
