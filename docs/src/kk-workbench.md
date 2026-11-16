---
description: Use KK Workbench for version control and file comparisons
title: KK Workbench
---

# KK Workbench

Open KK Workbench with {#action kk_workbench::ToggleFocus}. The workbench uses the KK executable configured by `kk_workbench.executable_path` and supports local and remote projects.

Use the workbench to inspect changes, open historical comparisons, review conflicts, and show KK blame annotations. Opening a working file preserves an existing comparison tab and reuses the ordinary editor tab for that file.

## Diff View Styles {#diff-view-styles}

Comparisons support split and unified layouts. Use the comparison toolbar to change the layout, or configure `diff_view_style` for the default. `minimum_split_diff_width` controls when a split comparison falls back to a unified layout.

## Word Diff Highlighting {#word-diff-highlighting}

`word_diff_enabled` controls highlighting of changed words inside diff hunks. The same display options apply to KK comparisons and clipboard comparisons.

## File Comparison Settings

The `diff` settings object controls the gutter, hunk style, and whether file comparisons initially show the full file. Scrollbar markers use `scrollbar.diff`, and gutter marker width uses `gutter.diff_gutter_width`.

Existing display preferences under the old `git` key are read for compatibility. New settings use the neutral names above.
