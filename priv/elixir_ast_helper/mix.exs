defmodule ElixirAstHelper.MixProject do
  use Mix.Project

  @version "0.1.0"

  def project do
    [
      app: :elixir_ast_helper,
      version: @version,
      elixir: "~> 1.15",
      start_permanent: false,
      escript: escript(),
      deps: deps(),
      description: "Daemon-managed Elixir AST helper for blackbox refactor tools (EX-G11/G12/G13).",
      package: package()
    ]
  end

  def application do
    [extra_applications: [:logger]]
  end

  defp escript do
    [main_module: ElixirAstHelper.CLI, name: "elixir_ast_helper"]
  end

  defp deps do
    [
      {:jason, "~> 1.4"}
    ]
  end

  defp package do
    [
      licenses: ["MIT"],
      links: %{}
    ]
  end
end
