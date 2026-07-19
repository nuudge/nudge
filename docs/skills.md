# Skills

Package reusable expertise as a folder: a `SKILL.md` (YAML frontmatter with `name` +
`description`, then markdown instructions) plus optional reference files and scripts.

Drop skills under `~/.nudge/skills/<name>/` (personal) or `.nudge/skills/<name>/` (project,
which wins on a name collision); they're discovered at startup.

Only each skill's name and description sit in context until the model decides one fits the
task and loads its full instructions on demand (via a `use_skill` tool) — so you can install
many skills with negligible context cost. The instructions can point at bundled files the
model reads with `Read` and scripts it runs with `Bash`.
