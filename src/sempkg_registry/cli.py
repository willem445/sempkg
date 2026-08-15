"""CLI entry point for sempkg_registry."""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path


def _cmd_serve(args: argparse.Namespace) -> None:
    admin_password = os.environ.get("sempkg_registry_ADMIN_PASSWORD")
    if not admin_password:
        sys.exit(
            "Error: sempkg_registry_ADMIN_PASSWORD environment variable is not set. "
            "Set it before starting the server."
        )

    try:
        import uvicorn
    except ImportError:
        sys.exit("Error: uvicorn is not installed. Install with: pip install uvicorn[standard]")

    from .auth import TokenStore
    from .storage import BundleStorage
    from .app import create_app

    storage_dir = Path(args.storage_dir) if args.storage_dir else None
    config_dir = Path(args.config_dir) if args.config_dir else None

    storage = BundleStorage(storage_dir=storage_dir)
    token_store = TokenStore(config_dir=config_dir)
    app = create_app(storage=storage, token_store=token_store, admin_password=admin_password)

    uvicorn.run(app, host=args.host, port=args.port)


def _cmd_token_add(args: argparse.Namespace) -> None:
    from .auth import TokenStore

    config_dir = Path(args.config_dir) if args.config_dir else None
    store = TokenStore(config_dir=config_dir)
    label = args.label or ""
    new_token = store.add_token(label=label)
    print(new_token.token)


def _cmd_token_list(args: argparse.Namespace) -> None:
    import json
    from .auth import TokenStore

    config_dir = Path(args.config_dir) if args.config_dir else None
    store = TokenStore(config_dir=config_dir)
    tokens = store.list_tokens()
    print(json.dumps(tokens, indent=2))


def _cmd_token_revoke(args: argparse.Namespace) -> None:
    from .auth import TokenStore

    config_dir = Path(args.config_dir) if args.config_dir else None
    store = TokenStore(config_dir=config_dir)
    if store.revoke_token(args.token):
        print("Token revoked.")
    else:
        sys.exit("Error: token not found.")


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="sempkg-registry",
        description="Self-hosted SemBundle Registry server",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    # ---- serve ----
    serve_parser = subparsers.add_parser("serve", help="Start the registry server")
    serve_parser.add_argument("--host", default="0.0.0.0", help="Bind host (default: 0.0.0.0)")
    serve_parser.add_argument("--port", type=int, default=8765, help="Bind port (default: 8765)")
    serve_parser.add_argument("--storage-dir", default=None, help="Path to bundle storage directory")
    serve_parser.add_argument("--config-dir", default=None, help="Path to config directory")
    serve_parser.set_defaults(func=_cmd_serve)

    # ---- token ----
    token_parser = subparsers.add_parser("token", help="Manage publish tokens")
    token_subparsers = token_parser.add_subparsers(dest="token_command", required=True)

    token_add = token_subparsers.add_parser("add", help="Create a new publish token")
    token_add.add_argument("--label", default="", help="Human-readable label for the token")
    token_add.add_argument("--config-dir", default=None, help="Path to config directory")
    token_add.set_defaults(func=_cmd_token_add)

    token_list = token_subparsers.add_parser("list", help="List all tokens")
    token_list.add_argument("--config-dir", default=None, help="Path to config directory")
    token_list.set_defaults(func=_cmd_token_list)

    token_revoke = token_subparsers.add_parser("revoke", help="Revoke a token")
    token_revoke.add_argument("token", help="Token value to revoke")
    token_revoke.add_argument("--config-dir", default=None, help="Path to config directory")
    token_revoke.set_defaults(func=_cmd_token_revoke)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
