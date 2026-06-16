"""Pytest configuration and shared fixtures."""

import sys
from pathlib import Path

# Add src directory to path so we can import sempkg_registry
project_root = Path(__file__).parent.parent
src_dir = project_root / "src"
if str(src_dir) not in sys.path:
    sys.path.insert(0, str(src_dir))
