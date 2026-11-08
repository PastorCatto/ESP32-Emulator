#!/usr/bin/env python3
"""Check that every relative link in the repository's Markdown files points at a file that exists.

Usage: python3 tools/check_links.py        (from anywhere; it works on the repository it lives in)
Skips vendor/, third-party/ (upstream files, copied unchanged),, target/ and links that start with a scheme (http:, https:, mailto:) or '#'.
Only checks that the file exists, not that a #fragment matches a heading.
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SKIP = {"vendor", "target", ".git", "third-party"}
LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")

bad = 0
checked = 0
for folder, dirs, files in os.walk(ROOT):
    dirs[:] = [d for d in dirs if d not in SKIP]
    for name in files:
        if not name.endswith(".md"):
            continue
        path = os.path.join(folder, name)
        in_code = False
        with open(path, encoding="utf-8") as fh:
            for no, line in enumerate(fh, 1):
                if line.lstrip().startswith("```"):
                    in_code = not in_code
                if in_code:
                    continue
                for target in LINK.findall(line):
                    if re.match(r"^[a-z][a-z0-9+.-]*:", target) or target.startswith("#"):
                        continue
                    checked += 1
                    dest = os.path.normpath(os.path.join(folder, target.split("#")[0]))
                    if not os.path.exists(dest):
                        bad += 1
                        print("%s:%d: broken link %s" % (os.path.relpath(path, ROOT), no, target))
print("%d links checked, %d broken" % (checked, bad))
sys.exit(1 if bad else 0)
