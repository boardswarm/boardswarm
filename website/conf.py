# Configuration file for the Sphinx documentation builder.
#
# For the full list of built-in configuration values, see the documentation:
# https://www.sphinx-doc.org/en/master/usage/configuration.html

# -- Project information -----------------------------------------------------
# https://www.sphinx-doc.org/en/master/usage/configuration.html#project-information

project = 'Boardswarm'
copyright = 'Boardswarm authors'
author = 'Boardswarm authors'


# -- General configuration ---------------------------------------------------
# https://www.sphinx-doc.org/en/master/usage/configuration.html#general-configuration

#extensions = []

templates_path = ['_templates']
exclude_patterns = ['_build', 'Thumbs.db', '.DS_Store']

# Add the extension
extensions = [
    "sphinx.ext.autosectionlabel",
    "sphinx.ext.intersphinx",
]

# Make sure the target is unique
autosectionlabel_prefix_document = True


# -- Options for HTML output -------------------------------------------------
# https://www.sphinx-doc.org/en/master/usage/configuration.html#options-for-html-output

html_theme = 'sphinx_rtd_theme'
html_static_path = ['_static']

html_theme_options = {
    'prev_next_buttons_location': None,
}

html_context = {
  'display_github': True,
  'github_user': 'boardswarm',
  'github_repo': 'boardswarm',
  'github_version': 'main/website/',
}
