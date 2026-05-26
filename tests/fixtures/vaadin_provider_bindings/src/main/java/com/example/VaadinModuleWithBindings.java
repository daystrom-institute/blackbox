package com.example;

import com.google.inject.AbstractModule;
import com.google.inject.Provider;
import com.google.inject.Provides;
import com.vaadin.flow.component.UI;
import com.vaadin.flow.server.VaadinSession;

/**
 * Guice module with both Vaadin provider bindings already present.
 * Used for idempotency/parity integration tests: a second macro run
 * must detect these methods and produce zero edits.
 */
public class VaadinModuleWithBindings extends AbstractModule {

    @Override
    protected void configure() {
        // configure Vaadin bindings here
    }

    @Provides
    Provider<UI> provideUiProvider() {
        return UI::getCurrent;
    }

    @Provides
    Provider<VaadinSession> provideVaadinSessionProvider() {
        return VaadinSession::getCurrent;
    }
}
