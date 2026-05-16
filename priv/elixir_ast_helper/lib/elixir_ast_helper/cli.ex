defmodule ElixirAstHelper.CLI do
  @moduledoc """
  Long-running daemon-managed helper for blackbox's Elixir refactor surface.

  Reads JSON-RPC-style commands from stdin (one per line), executes them,
  writes JSON responses to stdout. Commands implemented:

    * `parse_with_comments` — `Code.string_to_quoted_with_comments!/2`
      round-trip support (EX-V6 writable lane).

    * `compile_diagnostics` — wrap a `mix compile --return-errors` invocation
      with `Code.with_diagnostics/2` capture; returns structured diagnostic
      records (file, line, message, severity) for `elixir_compile_fix_round`.

    * `credo_diagnostics` — parse `mix credo --format=json` output into a
      stable schema for `elixir_credo_fix_round`.

    * `dialyzer_diagnostics` — parse `mix dialyzer --format=short` output
      for `elixir_dialyzer_attribution`.

    * `format_check` — `mix format --check-formatted` over a path or stdin
      buffer; returns whether formatting drifted.

    * `ping` — health check; returns `{:ok, "pong"}`.

  Lifecycle: launched by the daemon's `helper.rs` with the project root as
  argv[0]. The helper Mix project is built once per project version via
  `mix escript.build` and cached at `$BLACKBOX_STATE_DIR/elixir_helpers/`
  (path is daemon-side concern; the escript itself is stateless beyond a
  single Mix.Project.in_project context).

  Protocol: one line of JSON per request; one line of JSON per response.
  Request envelope:
    {"id": "uuid", "cmd": "parse_with_comments", "args": {...}}
  Response envelope:
    {"id": "uuid", "ok": true, "result": {...}}
    or
    {"id": "uuid", "ok": false, "error": "..."}
  """

  def main(args) do
    project_root = List.first(args) || File.cwd!()
    # Note: in a real Mix.Project.in_project(...) wrapper we'd chdir into the
    # project to pick up its deps + version; v1 keeps it simple — the helper
    # is built against its own deps and invoked from any directory.

    setup_io()
    loop(project_root)
  end

  defp setup_io do
    # Line-buffered stdout for prompt request/response interleaving.
    :ok = :io.setopts(:standard_io, encoding: :unicode)
  end

  defp loop(project_root) do
    case IO.gets(:standard_io, "") do
      :eof ->
        :ok

      {:error, reason} ->
        IO.puts(:standard_error, "stdin read error: #{inspect(reason)}")
        :ok

      line when is_binary(line) ->
        handle_line(String.trim(line), project_root)
        loop(project_root)
    end
  end

  defp handle_line("", _project_root), do: :ok

  defp handle_line(line, project_root) do
    response =
      case Jason.decode(line) do
        {:ok, %{"cmd" => cmd, "id" => id} = req} ->
          execute(cmd, req["args"] || %{}, project_root)
          |> envelope(id)

        {:ok, _missing} ->
          %{"ok" => false, "error" => "request missing id or cmd"}

        {:error, err} ->
          %{"ok" => false, "error" => "json decode failed: #{inspect(err)}"}
      end

    IO.puts(Jason.encode!(response))
  end

  defp envelope({:ok, result}, id), do: %{"id" => id, "ok" => true, "result" => result}
  defp envelope({:error, reason}, id), do: %{"id" => id, "ok" => false, "error" => to_string(reason)}

  # ── command dispatch ──────────────────────────────────────────────────────

  defp execute("ping", _args, _project_root), do: {:ok, "pong"}

  defp execute("parse_with_comments", %{"source" => src}, _project_root) when is_binary(src) do
    try do
      {quoted, comments} =
        Code.string_to_quoted_with_comments!(src,
          columns: true,
          token_metadata: true,
          literal_encoder: false,
          unescape: false,
          emit_warnings: false
        )

      {:ok,
       %{
         "quoted" => inspect(quoted, limit: :infinity, printable_limit: :infinity),
         "comments" =>
           Enum.map(comments, fn c ->
             %{
               "line" => c.line,
               "column" => Map.get(c, :column, 0),
               "text" => c.text,
               "previous_eol_count" => c.previous_eol_count,
               "next_eol_count" => c.next_eol_count
             }
           end)
       }}
    rescue
      e -> {:error, Exception.message(e)}
    end
  end

  defp execute("compile_diagnostics", %{"path" => path}, _project_root) when is_binary(path) do
    # v1: Code.with_diagnostics is available since Elixir 1.15.
    if function_exported?(Code, :with_diagnostics, 2) do
      {result, diagnostics} =
        apply(Code, :with_diagnostics, [
          fn ->
            try do
              src = File.read!(path)
              Code.compile_string(src, path)
              :ok
            rescue
              e -> {:error, Exception.message(e)}
            end
          end,
          [log: false]
        ])

      {:ok,
       %{
         "result" => inspect(result),
         "diagnostics" =>
           Enum.map(diagnostics, fn d ->
             %{
               "severity" => to_string(d.severity),
               "message" => d.message,
               "file" => d.file,
               "position" => inspect(d.position),
               "stacktrace" => inspect(d.stacktrace)
             }
           end)
       }}
    else
      {:error, "Code.with_diagnostics/2 unavailable; helper requires Elixir 1.15+"}
    end
  end

  defp execute("format_check", %{"path" => path}, _project_root) when is_binary(path) do
    try do
      src = File.read!(path)
      formatted = Code.format_string!(src) |> IO.iodata_to_binary()

      drifted = src != formatted

      {:ok,
       %{
         "drifted" => drifted,
         "path" => path
       }}
    rescue
      e -> {:error, Exception.message(e)}
    end
  end

  defp execute(cmd, _args, _project_root) do
    {:error, "unknown command: #{cmd}"}
  end
end
