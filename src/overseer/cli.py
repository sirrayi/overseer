"""Overseer CLI (plan B2 guidance, B0 scope: init, doctor, --version).

Lazy imports keep startup fast. Business logic lives in modules, not here.
"""

from __future__ import annotations

import typer
from rich.console import Console

from overseer import __version__

app = typer.Typer(
    name="overseer",
    help="Vault-native, self-improving, verification-driven agent harness.",
    no_args_is_help=True,
)
console = Console()


def _version_callback(value: bool) -> None:
    if value:
        console.print(f"overseer {__version__}")
        raise typer.Exit()


@app.callback()
def main(
    version: bool = typer.Option(
        False,
        "--version",
        "-V",
        help="Print version and exit.",
        is_eager=True,
        callback=_version_callback,
    ),
) -> None:
    """Overseer — the engine. Everything."""


@app.command()
def init(
    vault_path: str = typer.Option(
        "~/overseer-vault", "--vault", "-v", help="Path to the canonical vault."
    ),
    config: str = typer.Option(
        "config.yaml", "--config", "-c", help="Path to config file (created if missing)."
    ),
) -> None:
    """Create a compliant vault and sample config (plan Part 3: overseer init)."""
    from overseer.config import write_sample_config
    from overseer.errors import ConfigError
    from overseer.vault import Vault

    vault = Vault(vault_path)
    created = vault.init()
    try:
        write_sample_config(config, vault_path=vault_path)
    except ConfigError as exc:
        console.print(f"[yellow]{exc}[/yellow]")
        console.print("[yellow]config left untouched; vault is ready[/yellow]")
    console.print(f"[green]vault ready:[/green] {vault.root}")
    console.print(f"[green]created {len(created)} system/template notes[/green]")
    console.print(
        f"[green]sample config:[/green] {config} (placeholders only — set secrets via env)"
    )
    console.print("[yellow]next:[/yellow] overseer doctor")


@app.command()
def doctor(
    config: str = typer.Option("config.yaml", "--config", "-c", help="Path to config file."),
) -> None:
    """Validate config, vault, provider, and permissions."""
    from overseer.config import load_config
    from overseer.doctor import run_doctor
    from overseer.errors import ConfigError

    try:
        cfg = load_config(config)
    except ConfigError as exc:
        console.print(f"[red]config failed to load:[/red] {exc}")
        raise typer.Exit(code=1) from exc

    report = run_doctor(cfg)
    console.print(report.render())
    if not report.ok:
        raise typer.Exit(code=1)


@app.command()
def version() -> None:
    """Print the overseer version."""
    console.print(f"overseer {__version__}")


if __name__ == "__main__":
    app()
