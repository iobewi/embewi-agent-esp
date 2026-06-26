# Configuration Sphinx — documentation Embewi Agent.
# Markdown consommé directement via MyST-Parser (aucune réécriture des .md).

project = "Embewi Agent"
author = "Embewi"
copyright = "2026, Embewi"
language = "fr"

extensions = ["myst_parser"]

# Markdown → pages. heading_anchors : ancres h1–h3 pour les liens inter-sections.
source_suffix = {".md": "markdown"}
myst_enable_extensions = ["colon_fence", "deflist"]
myst_heading_anchors = 3

# Fichiers du dossier docs/ à NE PAS traiter comme des pages.
exclude_patterns = ["_build", "requirements.txt", "conf.py", "Thumbs.db", ".DS_Store"]

html_theme = "furo"
html_title = "Embewi Agent — Documentation"
