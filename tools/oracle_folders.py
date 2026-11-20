"""A folder of its own for each vanilla oracle call a harness makes.

tools/parity_shim.py replays an oracle call only while the files under the paths the call names are the ones it
recorded: its inputs with the same content and no file added beside them, and its outputs exactly as the call left
them (size and mtime). A harness that hands an oracle its whole work folder breaks that on every run: its later steps
add files there (the Rust side's output, logs, report.json, the next call's output), and a log it opens there before
the call is one of the call's outputs, changed again.

folder() gives a call a folder that holds only the inputs it is given, copied in, and what the call writes there. The
call's log, the Rust side's files and the report stay outside it. The folder is kept between runs and never emptied:
a replay writes no files, so the call's outputs must still be there as it left them.
"""

from pathlib import Path
import shutil


def folder(path, *inputs):
    """`path`, made if missing, holding a copy of each of `inputs` under its own name."""
    path = Path(path)
    path.mkdir(parents=True, exist_ok=True)
    for source in inputs:
        shutil.copyfile(source, path / Path(source).name)
    return path


def copy(path, target, *names):
    """Copy the files `names` a call wrote in its folder `path` to `target`, for the harness's later steps to read
    there: what they write beside them then stays out of the call's folder."""
    for name in names:
        shutil.copyfile(Path(path) / name, Path(target) / name)
