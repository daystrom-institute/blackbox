package com.blackbox.javaworker;

import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.PropertyNamingStrategies;
import com.fasterxml.jackson.databind.node.ObjectNode;
import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;

/**
 * Blackbox Java Worker — JSON-RPC 2.0 sidecar for Java source synthesis.
 *
 * <p>Reads one JSON-RPC request per line from stdin, dispatches to the
 * appropriate handler, and writes one JSON-RPC response per line to stdout.
 * Shuts down cleanly on EOF or a {@code shutdown} request.
 *
 * <p>CRITICAL INVARIANT: This process is filesystem-free. All source content
 * arrives via request params; all output goes via JSON response. The Rust
 * client owns file I/O and transaction management.
 */
public final class Main {

    private static final ObjectMapper MAPPER = new ObjectMapper()
            .configure(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES, false)
            .setPropertyNamingStrategy(PropertyNamingStrategies.SNAKE_CASE);

    private Main() {}

    public static void main(String[] args) throws IOException {
        Dispatcher dispatcher = new Dispatcher(MAPPER);

        try (BufferedReader reader = new BufferedReader(
                new InputStreamReader(System.in, StandardCharsets.UTF_8))) {

            String line;
            while ((line = reader.readLine()) != null) {
                line = line.trim();
                if (line.isEmpty()) {
                    continue;
                }

                RpcResponse response;
                try {
                    RpcRequest request = MAPPER.readValue(line, RpcRequest.class);
                    response = dispatcher.dispatch(request);
                } catch (Exception e) {
                    // JSON parse error
                    ObjectNode data = MAPPER.createObjectNode();
                    data.put("details", e.getMessage() != null ? e.getMessage() : e.getClass().getName());
                    // Request was unparseable, so no id is available; 0 is the
                    // JSON-RPC sentinel for "unknown id" on a parse error.
                    response = RpcResponse.error(0L, -32700, "Parse error", data);
                }

                String json = MAPPER.writeValueAsString(response);
                System.out.println(json);
                System.out.flush();

                // Stop reading after shutdown request — the handler sends
                // {ok: true} before we break.
                if (response.getError() == null
                        && response.getResult() != null
                        && response.getResult().has("ok")
                        && response.getResult().get("ok").asBoolean(false)) {
                    // Only shutdown returns {ok: true}, but verify via the
                    // original request method to be safe.
                    // We check the result structure; the dispatch guarantees
                    // only shutdown produces this shape.
                    break;
                }
            }
        }

        System.exit(0);
    }
}
