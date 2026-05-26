package com.blackbox.javaworker;

import java.io.InputStream;
import java.util.Properties;

/**
 * Reads the version of OpenRewrite from bundled package metadata.
 *
 * <p>Strategy (tried in order):
 * <ol>
 *   <li>{@code META-INF/blackbox-java-worker.properties} — Maven resource-filtered
 *       at build time; {@code openrewrite.version=${rewrite.version}} is replaced
 *       with the real version. Most reliable in a shaded jar.</li>
 *   <li>{@code META-INF/maven/org.openrewrite/rewrite-core/pom.properties} —
 *       present when the shade plugin preserves dependency Maven metadata.</li>
 *   <li>{@code org.openrewrite.Recipe} package implementation version — reads
 *       {@code Implementation-Version} from the component jar's MANIFEST.MF;
 *       works in unshaded test environments.</li>
 * </ol>
 * Falls back to {@code "unknown"} if none of the above is available.
 */
final class OpenRewriteVersion {

    private static final String UNKNOWN = "unknown";

    private OpenRewriteVersion() {}

    /** Returns the OpenRewrite core version string, or {@code "unknown"}. */
    static String get() {
        // 1. Build-time injected properties file (most reliable in fat jar).
        String v = tryReadVersionProperty(
                "META-INF/blackbox-java-worker.properties", "openrewrite.version");
        if (v != null) return v;

        // 2. rewrite-core Maven metadata (present when shade preserves META-INF/maven).
        v = tryReadVersionProperty(
                "META-INF/maven/org.openrewrite/rewrite-core/pom.properties", "version");
        if (v != null) return v;

        // 3. Package implementation version from the Recipe class (unshaded jar only).
        try {
            Package pkg = org.openrewrite.Recipe.class.getPackage();
            if (pkg != null) {
                String iv = pkg.getImplementationVersion();
                if (iv != null && !iv.isBlank()) return iv.trim();
            }
        } catch (Exception ignored) { /* fall through */ }

        return UNKNOWN;
    }

    private static String tryReadVersionProperty(String resourcePath, String key) {
        try (InputStream is = OpenRewriteVersion.class.getClassLoader()
                .getResourceAsStream(resourcePath)) {
            if (is == null) return null;
            Properties props = new Properties();
            props.load(is);
            String version = props.getProperty(key);
            if (version != null && !version.isBlank()
                    && !version.startsWith("${")) {  // guard against un-filtered placeholder
                return version.trim();
            }
        } catch (Exception ignored) { /* fall through */ }
        return null;
    }
}
