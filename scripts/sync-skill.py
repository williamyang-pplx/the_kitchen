#!/usr/bin/env python3
"""Regenerate skill/kitchen/SKILL.md from the `kitchenctl guide` text."""
import pathlib

root = pathlib.Path(__file__).resolve().parent.parent
guide = (root / "crates/kitchenctl/src/guide.md").read_text()
front = """---
name: kitchen
description: Drive the kitchen (William's cmux-style workspace manager) with kitchenctl — identify your workspace/tab, open tabs, and run the planning loop (plan file, comments, resolve). Use whenever KITCHEN_TAB_ID is set, or the user mentions the kitchen, kitchen tabs, or kitchenctl.
---
"""
out = root / "skill/kitchen/SKILL.md"
out.parent.mkdir(parents=True, exist_ok=True)
out.write_text(front + guide)
print(f"wrote {out}")
