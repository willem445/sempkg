"""Package registry — stores and manages registered internal packages."""

import json
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Optional

CONFIG_DIR = Path.home() / ".codegraph-hub"
CONFIG_FILE = CONFIG_DIR / "packages.json"


@dataclass
class Package:
    name: str
    path: str
    description: str = ""

    @property
    def abs_path(self) -> Path:
        return Path(self.path).expanduser().resolve()

    @property
    def is_indexed(self) -> bool:
        return (self.abs_path / ".codegraph").exists()


class Registry:
    def __init__(self) -> None:
        CONFIG_DIR.mkdir(parents=True, exist_ok=True)
        self._packages: dict[str, Package] = {}
        self._load()

    def _load(self) -> None:
        if CONFIG_FILE.exists():
            data = json.loads(CONFIG_FILE.read_text(encoding="utf-8"))
            self._packages = {name: Package(**pkg) for name, pkg in data.items()}

    def _save(self) -> None:
        CONFIG_FILE.write_text(
            json.dumps(
                {name: asdict(pkg) for name, pkg in self._packages.items()},
                indent=2,
            ),
            encoding="utf-8",
        )

    def add(self, name: str, path: str, description: str = "") -> Package:
        abs_path = Path(path).expanduser().resolve()
        if not abs_path.exists():
            raise ValueError(f"Path does not exist: {abs_path}")
        pkg = Package(name=name, path=str(abs_path), description=description)
        self._packages[name] = pkg
        self._save()
        return pkg

    def get(self, name: str) -> Optional[Package]:
        return self._packages.get(name)

    def list_all(self) -> list[Package]:
        return list(self._packages.values())

    def remove(self, name: str) -> bool:
        if name in self._packages:
            del self._packages[name]
            self._save()
            return True
        return False
