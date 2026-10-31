# Windows Alpha packaging

Run scripts/package-windows.ps1 with a new OutputDir. Use -SkipInstaller for a
portable ZIP, or NSIS 3.11 for the current-user installer. The script builds the
app and launcher once, checks dependency notices, includes matching source and
dependency sources, and records payload hashes and the exact source revision.

Packages are unsigned. Production update trust remains disabled. Packaging
does not launch the UI or installer, establish hardware acceptance, or publish.
Keep validation claims specific to checks actually run on each artifact.
