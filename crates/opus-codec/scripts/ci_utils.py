import os
import subprocess
import sys
from contextlib import contextmanager
from pathlib import Path
from typing import Dict, List, Optional, Union

@contextmanager
def group(name: str):
    """Group output in GitHub Actions."""
    # Only print group markers if running in GitHub Actions or if forced
    # But for now, we'll always print them as they are harmless in local terminals
    print(f"::group::{name}")
    try:
        yield
    finally:
        print("::endgroup::")

def run(
    cmd: List[str],
    env: Optional[Dict[str, str]] = None,
    cwd: Optional[Union[str, Path]] = None,
    check: bool = True,
    capture_output: bool = False,
) -> subprocess.CompletedProcess:
    """Run a command with optional grouping and error handling."""
    cmd_str = " ".join(str(c) for c in cmd)
    
    # Don't group if capturing output, as it's likely an internal check
    should_group = not capture_output
    
    if should_group:
        print(f"::group::{cmd_str}")
    
    try:
        run_env = os.environ.copy()
        if env:
            run_env.update(env)
            
        result = subprocess.run(
            cmd,
            env=run_env,
            cwd=cwd,
            check=check,
            text=True,
            capture_output=capture_output
        )
        return result
    except subprocess.CalledProcessError as e:
        if should_group:
            print(f"Command failed with exit code {e.returncode}")
        raise
    finally:
        if should_group:
            print("::endgroup::")

def fail(msg: str) -> None:
    """Exit with an error message."""
    sys.exit(f"Error: {msg}")
