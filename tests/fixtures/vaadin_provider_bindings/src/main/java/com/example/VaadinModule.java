package com.example;

import com.google.inject.AbstractModule;
import com.vaadin.flow.component.UI;
import com.vaadin.flow.server.VaadinSession;

/**
 * Guice module for Vaadin request-scoped objects.
 * No provider bindings present initially; they are added by the macro.
 */
public class VaadinModule extends AbstractModule {

    @Override
    protected void configure() {
        // configure Vaadin bindings here
    }
}
