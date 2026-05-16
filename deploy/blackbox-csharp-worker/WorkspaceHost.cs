// SPDX-FileCopyrightText: blackbox refactor track
// Owns the MSBuildWorkspace + Solution snapshot + per-run transaction state.

using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.Diagnostics;
using Microsoft.CodeAnalysis.MSBuild;
using Microsoft.CodeAnalysis.Text;

namespace Blackbox.CSharpWorker;

public sealed class WorkspaceHost : IAsyncDisposable
{
    private MSBuildWorkspace? _workspace;
    private Solution? _solution;
    private readonly List<string> _workspaceWarnings = new();
    private readonly Dictionary<string, Solution> _transactionSnapshots = new();
    private readonly Dictionary<string, GeneratorDriver?> _generatorDrivers = new();
    private string? _loadedFilePath;

    /// <summary>Files declared during a solution/project load — drives RX-V5 expected-vs-loaded.</summary>
    private readonly List<string> _expectedProjectNames = new();

    public IReadOnlyList<string> WorkspaceWarnings => _workspaceWarnings;

    public async Task<LoadResult> LoadAsync(string path, bool reset, CancellationToken ct)
    {
        if (reset || _workspace is null)
        {
            _workspace?.Dispose();
            _workspaceWarnings.Clear();
            _expectedProjectNames.Clear();
            _workspace = MSBuildWorkspace.Create();
            _workspace.WorkspaceFailed += (_, e) =>
            {
                _workspaceWarnings.Add($"[{e.Diagnostic.Kind}] {e.Diagnostic.Message}");
            };
        }

        _loadedFilePath = path;
        var lower = path.ToLowerInvariant();
        if (lower.EndsWith(".sln") || lower.EndsWith(".slnx"))
        {
            ParseExpectedProjectsFromSolution(path);
            _solution = await _workspace!.OpenSolutionAsync(path, cancellationToken: ct);
        }
        else if (lower.EndsWith(".csproj"))
        {
            var project = await _workspace!.OpenProjectAsync(path, cancellationToken: ct);
            _solution = project.Solution;
            _expectedProjectNames.Add(project.Name);
        }
        else
        {
            throw new InvalidOperationException($"unsupported workspace extension: {path}");
        }

        var loaded = ListLoadedProjects();
        var dropped = _expectedProjectNames
            .Where(name => !loaded.Any(lp => lp.Name == name))
            .ToList();
        return new LoadResult(loaded, dropped, _workspaceWarnings.ToList());
    }

    public LoadStatusResult GetLoadStatus()
    {
        if (_solution is null)
        {
            return new LoadStatusResult([], [], [], [], []);
        }
        var loaded = ListLoadedProjects();
        var dropped = _expectedProjectNames
            .Where(name => !loaded.Any(lp => lp.Name == name))
            .ToList();
        var degraded = _workspaceWarnings
            .Where(w => w.StartsWith("[Failure]"))
            .Select(w => w.Replace("[Failure] ", "").Split(':', 2)[0])
            .Distinct()
            .ToList();
        var warnings = _workspaceWarnings
            .Where(w => w.StartsWith("[Warning]"))
            .ToList();
        return new LoadStatusResult(_expectedProjectNames.ToList(), loaded, dropped, degraded, warnings);
    }

    public async Task<GetDiagnosticsResult> GetDiagnosticsAsync(string? file, bool includeAnalyzers, CancellationToken ct)
    {
        if (_solution is null)
        {
            return new GetDiagnosticsResult([], false);
        }
        var output = new List<SidecarDiagnostic>();
        var includedAnalyzers = false;
        if (file is not null)
        {
            var docId = _solution.GetDocumentIdsWithFilePath(file).FirstOrDefault();
            if (docId is not null)
            {
                var doc = _solution.GetDocument(docId)!;
                var model = await doc.GetSemanticModelAsync(ct);
                if (model is not null)
                {
                    foreach (var d in model.GetDiagnostics(cancellationToken: ct))
                    {
                        output.Add(ToSidecar(d, "compiler"));
                    }
                }
            }
        }
        else
        {
            foreach (var project in _solution.Projects)
            {
                var compilation = await project.GetCompilationAsync(ct);
                if (compilation is null) continue;
                foreach (var d in compilation.GetDiagnostics(ct))
                {
                    output.Add(ToSidecar(d, "compiler"));
                }
                // Generator-reported diagnostics live in
                // GeneratorDriverRunResult.Diagnostics. We re-run the
                // driver on demand because Roslyn doesn't expose them
                // through Compilation.GetDiagnostics. The driver
                // result is cached per project.
                if (TryGetGeneratorDriver(project) is { } driver)
                {
                    var (_, _, generatorDiags) = RunDriver(driver, compilation, ct);
                    foreach (var gd in generatorDiags)
                    {
                        output.Add(ToSidecar(gd, "generator"));
                    }
                }
                if (includeAnalyzers && project.AnalyzerReferences.Count > 0)
                {
                    var analyzers = project.AnalyzerReferences
                        .SelectMany(r => r.GetAnalyzers(LanguageNames.CSharp))
                        .ToImmutableArray();
                    if (analyzers.Length > 0)
                    {
                        var withAnalyzers = compilation.WithAnalyzers(analyzers, project.AnalyzerOptions);
                        var analyzerDiags = await withAnalyzers.GetAllDiagnosticsAsync(ct);
                        foreach (var d in analyzerDiags)
                        {
                            output.Add(ToSidecar(d, "analyzer"));
                        }
                        includedAnalyzers = true;
                    }
                }
            }
        }
        return new GetDiagnosticsResult(output, includedAnalyzers);
    }

    public bool UpdateDocumentText(string file, string newContent)
    {
        if (_solution is null) return false;
        var docId = _solution.GetDocumentIdsWithFilePath(file).FirstOrDefault();
        if (docId is null) return false;
        _solution = _solution.WithDocumentText(docId, SourceText.From(newContent));
        return true;
    }

    public TransactionResult BeginTransaction(string runId)
    {
        if (_solution is null)
        {
            return new TransactionResult(false, "no solution loaded");
        }
        // Solution is immutable; the snapshot is just a reference copy.
        _transactionSnapshots[runId] = _solution;
        return new TransactionResult(true, null);
    }

    public TransactionResult ApplyPlanStep(ApplyPlanStepParams p)
    {
        if (_solution is null)
        {
            return new TransactionResult(false, "no solution loaded");
        }
        foreach (var edit in p.Edits)
        {
            UpdateDocumentText(edit.File, edit.NewContent);
        }
        foreach (var created in p.Created)
        {
            // Find a project to add the document to — prefer the
            // project whose directory contains the file.
            var project = _solution.Projects
                .FirstOrDefault(pr => pr.FilePath is not null
                    && created.StartsWith(System.IO.Path.GetDirectoryName(pr.FilePath)!,
                        StringComparison.OrdinalIgnoreCase));
            if (project is null) continue;
            if (!System.IO.File.Exists(created)) continue;
            var content = System.IO.File.ReadAllText(created);
            var name = System.IO.Path.GetFileName(created);
            _solution = _solution.AddDocument(
                DocumentId.CreateNewId(project.Id),
                name,
                SourceText.From(content),
                filePath: created
            );
        }
        foreach (var deleted in p.Deleted)
        {
            var docId = _solution.GetDocumentIdsWithFilePath(deleted).FirstOrDefault();
            if (docId is not null)
            {
                _solution = _solution.RemoveDocument(docId);
            }
        }
        foreach (var move in p.FileMoves)
        {
            var docId = _solution.GetDocumentIdsWithFilePath(move.SourcePath).FirstOrDefault();
            if (docId is null) continue;
            var doc = _solution.GetDocument(docId)!;
            _solution = _solution.WithDocumentFilePath(docId, move.TargetPath)
                                 .WithDocumentName(docId, System.IO.Path.GetFileName(move.TargetPath));
            _ = doc;
        }
        return new TransactionResult(true, null);
    }

    public TransactionResult ApplyCommandTouches(ApplyCommandTouchesParams p)
    {
        if (_solution is null)
        {
            return new TransactionResult(false, "no solution loaded");
        }
        foreach (var path in p.Touches)
        {
            if (!System.IO.File.Exists(path)) continue;
            var content = System.IO.File.ReadAllText(path);
            UpdateDocumentText(path, content);
        }
        return new TransactionResult(true, null);
    }

    public TransactionResult CommitTransaction(string runId)
    {
        _transactionSnapshots.Remove(runId);
        return new TransactionResult(true, null);
    }

    public TransactionResult RollbackTransaction(string runId)
    {
        if (_transactionSnapshots.TryGetValue(runId, out var snapshot))
        {
            _solution = snapshot;
            _transactionSnapshots.Remove(runId);
            return new TransactionResult(true, null);
        }
        return new TransactionResult(false, $"no transaction with run_id={runId}");
    }

    public EnumerateGeneratorsResult EnumerateGenerators()
    {
        // v1 generator discovery: walk AnalyzerReferences on every
        // loaded project, pull out IIncrementalGenerator types via
        // reflection. We don't yet inspect the in-repo
        // *.SourceGenerators/ project's syntax (that's the Phase 2
        // RX-V4 follow-up); analyzer-reference discovery covers
        // package-shipped generators like Wolverine v5.
        var found = new List<DiscoveredGenerator>();
        if (_solution is null)
        {
            return new EnumerateGeneratorsResult(found);
        }
        foreach (var project in _solution.Projects)
        {
            foreach (var reference in project.AnalyzerReferences)
            {
                var generators = reference.GetGenerators(LanguageNames.CSharp);
                foreach (var gen in generators)
                {
                    var t = gen.GetType();
                    var asm = t.Assembly.GetName();
                    // Classification is "unknown" by default — without
                    // source inspection we can't tell metadata-name
                    // vs raw provider apart. The Rust side treats
                    // unknown as undetected for RX-V4 fail-closed.
                    var fingerprint = ComputeFingerprint(t, asm);
                    var source = reference.Display is { } d && d.Contains("SourceGenerators", StringComparison.OrdinalIgnoreCase)
                        ? "in_repo"
                        : "analyzer_reference";
                    found.Add(new DiscoveredGenerator(
                        Name: t.FullName ?? t.Name,
                        AssemblyIdentity: asm.ToString(),
                        SourcePath: reference.FullPath,
                        Classification: "unknown",
                        Attributes: Array.Empty<string>(),
                        Fingerprint: fingerprint,
                        Source: source
                    ));
                }
            }
        }
        return new EnumerateGeneratorsResult(found);
    }

    public ValueTask DisposeAsync()
    {
        _workspace?.Dispose();
        return ValueTask.CompletedTask;
    }

    // ── helpers ─────────────────────────────────────────────────

    private IReadOnlyList<LoadedProject> ListLoadedProjects()
    {
        if (_solution is null) return [];
        return _solution.Projects
            .Select(p => new LoadedProject(p.Name, p.Documents.Count(), p.FilePath))
            .OrderBy(p => p.Name)
            .ToList();
    }

    private void ParseExpectedProjectsFromSolution(string path)
    {
        try
        {
            var text = System.IO.File.ReadAllText(path);
            if (path.EndsWith(".sln", StringComparison.OrdinalIgnoreCase))
            {
                foreach (var line in text.Split('\n'))
                {
                    var trimmed = line.TrimStart();
                    if (!trimmed.StartsWith("Project(")) continue;
                    var eq = trimmed.IndexOf('=');
                    if (eq < 0) continue;
                    var parts = SplitQuotedCsv(trimmed[(eq + 1)..]);
                    if (parts.Count >= 2 && parts[1].EndsWith(".csproj", StringComparison.OrdinalIgnoreCase))
                    {
                        _expectedProjectNames.Add(parts[0]);
                    }
                }
            }
            else
            {
                // .slnx
                var idx = 0;
                while (true)
                {
                    var open = text.IndexOf("<Project", idx, StringComparison.Ordinal);
                    if (open < 0) break;
                    var close = text.IndexOf('>', open);
                    if (close < 0) break;
                    var tag = text[open..(close + 1)];
                    idx = close + 1;
                    var attrIdx = tag.IndexOf("Path=\"", StringComparison.Ordinal);
                    if (attrIdx < 0) continue;
                    var pathStart = attrIdx + "Path=\"".Length;
                    var pathEnd = tag.IndexOf('"', pathStart);
                    if (pathEnd < 0) continue;
                    var p = tag[pathStart..pathEnd];
                    if (p.EndsWith(".csproj", StringComparison.OrdinalIgnoreCase))
                    {
                        var name = System.IO.Path.GetFileNameWithoutExtension(p);
                        _expectedProjectNames.Add(name);
                    }
                }
            }
        }
        catch
        {
            // best-effort parse; missing expected list only weakens
            // RX-V5 — the workspace-load itself still happens.
        }
    }

    private static List<string> SplitQuotedCsv(string s)
    {
        var output = new List<string>();
        var current = new System.Text.StringBuilder();
        var inQuotes = false;
        foreach (var ch in s)
        {
            if (ch == '"')
            {
                inQuotes = !inQuotes;
            }
            else if (ch == ',' && !inQuotes)
            {
                output.Add(current.ToString().Trim());
                current.Clear();
            }
            else if (inQuotes)
            {
                current.Append(ch);
            }
        }
        if (inQuotes || current.Length > 0)
        {
            output.Add(current.ToString().Trim());
        }
        return output;
    }

    private static SidecarDiagnostic ToSidecar(Diagnostic d, string origin)
    {
        var span = d.Location.GetMappedLineSpan();
        return new SidecarDiagnostic(
            Code: d.Id,
            Severity: d.Severity switch
            {
                DiagnosticSeverity.Error => "error",
                DiagnosticSeverity.Warning => "warning",
                DiagnosticSeverity.Info => "info",
                _ => "hidden",
            },
            Message: d.GetMessage(),
            File: span.Path,
            Line: span.StartLinePosition.Line,
            Character: span.StartLinePosition.Character,
            EndLine: span.EndLinePosition.Line,
            EndCharacter: span.EndLinePosition.Character,
            Origin: origin
        );
    }

    private GeneratorDriver? TryGetGeneratorDriver(Project project)
    {
        if (_generatorDrivers.TryGetValue(project.Name, out var cached))
        {
            return cached;
        }
        var generators = project.AnalyzerReferences
            .SelectMany(r => r.GetGenerators(LanguageNames.CSharp))
            .ToImmutableArray();
        if (generators.Length == 0)
        {
            _generatorDrivers[project.Name] = null;
            return null;
        }
        var parseOptions = project.ParseOptions as Microsoft.CodeAnalysis.CSharp.CSharpParseOptions
                           ?? Microsoft.CodeAnalysis.CSharp.CSharpParseOptions.Default;
        var driver = Microsoft.CodeAnalysis.CSharp.CSharpGeneratorDriver.Create(
            generators,
            parseOptions: parseOptions
        );
        _generatorDrivers[project.Name] = driver;
        return driver;
    }

    private static (GeneratorDriver, Compilation, IReadOnlyList<Diagnostic>) RunDriver(
        GeneratorDriver driver,
        Compilation compilation,
        CancellationToken ct
    )
    {
        var updated = driver.RunGeneratorsAndUpdateCompilation(
            compilation,
            out var outCompilation,
            out var diags,
            ct
        );
        return (updated, outCompilation, diags.ToList());
    }

    private static string ComputeFingerprint(Type type, System.Reflection.AssemblyName asm)
    {
        var input = $"{type.FullName}|{asm.Name}|{asm.Version}|{System.Convert.ToHexString(asm.GetPublicKeyToken() ?? Array.Empty<byte>())}";
        var bytes = System.Text.Encoding.UTF8.GetBytes(input);
        var hash = System.Security.Cryptography.SHA256.HashData(bytes);
        return "sha256:" + Convert.ToHexString(hash).ToLowerInvariant()[..16];
    }
}

internal static class ImmutableArrayExtensions
{
    public static System.Collections.Immutable.ImmutableArray<T> ToImmutableArray<T>(this IEnumerable<T> source)
        => System.Collections.Immutable.ImmutableArray.CreateRange(source);
}
