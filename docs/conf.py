# Configuration Sphinx — documentation de l'agent Embewi (ESP32).
# Markdown via MyST. Site distinct du hub `embewi` (qui porte le contrat).

project = "Embewi Agent"
author = "Embewi"
copyright = "2026, Embewi"
language = "fr"

# i18n : français = langue source, anglais = traduction via gettext/sphinx-intl.
#   sphinx-build -b gettext docs docs/_build/gettext   → extrait les .pot
#   sphinx-intl update -p docs/_build/gettext -l en    → génère locale/en/LC_MESSAGES/*.po
#   (traduire les .po)
#   sphinx-intl build -d locale                        → compile en .mo
#   sphinx-build -b html -D language=en docs docs/_build/html/en
locale_dirs = ["locale/"]
gettext_compact = True

extensions = ["myst_parser", "sphinx_rtd_theme", "sphinxcontrib.mermaid"]

source_suffix = {".md": "markdown"}
myst_enable_extensions = ["colon_fence", "deflist"]
myst_heading_anchors = 3

exclude_patterns = ["_build", "requirements.txt", "conf.py", "Thumbs.db", ".DS_Store"]

html_theme = "sphinx_rtd_theme"
html_title = "Embewi Agent — Documentation"
html_theme_options = {
    "navigation_depth": 3,
    "collapse_navigation": False,
    "style_external_links": True,
}
html_static_path = ["_static"]
html_logo = "_static/logo.png"
html_favicon = "_static/favicon.ico"
