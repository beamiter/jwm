from pathlib import Path
import ast
import base64
import gzip
import json
import subprocess

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ".github/jwm-capture-apply.py"

def load_payload() -> str:
    revisions = subprocess.check_output(
        ["git", "rev-list", "HEAD", "--", SCRIPT],
        cwd=ROOT,
        text=True,
    ).splitlines()
    for revision in revisions:
        try:
            source = subprocess.check_output(
                ["git", "show", f"{revision}:{SCRIPT}"],
                cwd=ROOT,
                text=True,
                stderr=subprocess.DEVNULL,
            )
            module = ast.parse(source)
        except (subprocess.CalledProcessError, SyntaxError):
            continue
        for node in module.body:
            if not isinstance(node, ast.Assign):
                continue
            if not any(
                isinstance(target, ast.Name) and target.id == "PAYLOAD"
                for target in node.targets
            ):
                continue
            if isinstance(node.value, ast.Constant) and isinstance(node.value.value, str):
                if len(node.value.value) > 1000:
                    return node.value.value
    raise RuntimeError("could not recover the staged capture payload from git history")

PAYLOAD = load_payload()

def indent_block(text: str, prefix: str) -> str:
    return "".join(
        prefix + line if line.strip() else line
        for line in text.splitlines(keepends=True)
    )

def replace_once(data: str, old: str, new: str, path: str, index: int) -> str:
    candidates = [(old, new, 0)]
    candidates.extend(
        (
            indent_block(old, " " * width),
            indent_block(new, " " * width),
            width,
        )
        for width in range(4, 65, 4)
    )
    matches = [
        (candidate_old, candidate_new, width)
        for candidate_old, candidate_new, width in candidates
        if data.count(candidate_old) == 1
    ]
    if not matches:
        nonzero = [
            (width, data.count(candidate_old))
            for candidate_old, _, width in candidates
            if data.count(candidate_old)
        ]
        raise RuntimeError(
            f"{path} replacement #{index}: expected one match, candidates={nonzero or 'none'}"
        )
    candidate_old, candidate_new, _ = max(matches, key=lambda match: match[2])
    return data.replace(candidate_old, candidate_new, 1)

payload = json.loads(gzip.decompress(base64.b64decode(PAYLOAD)).decode("utf-8"))
(ROOT / "src/jwm/features/capture.rs").write_text(payload["capture_rs"], encoding="utf-8")

for index, (path, old, new) in enumerate(payload["replacements"]):
    file_path = ROOT / path
    data = file_path.read_text(encoding="utf-8")
    file_path.write_text(
        replace_once(data, old, new, path, index),
        encoding="utf-8",
    )

print(f"Applied {len(payload['replacements'])} source transformations")
