import sys
from pathlib import Path

# adapters/hermes/ is the plugin package ("hermes"), loaded by Hermes itself from
# ~/.hermes/plugins/<name>/ the same way ~/.hermes/plugins/orca-status/ is. Tests live
# one level down (adapters/hermes/tests/), so the plugin's parent directory
# (adapters/) needs to be importable as the place "hermes" lives.
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
