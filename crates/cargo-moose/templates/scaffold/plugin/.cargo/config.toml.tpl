# Local build-environment config. Gitignored - set your own values.
# `cargo moose install` and `cargo moose package` both read from here.
#
# Cargo injects everything in `[env]` into the environment of any
# subcommand it spawns, so values here are visible to `cargo moose`
# without further plumbing. See https://truce.audio/ for the full
# list of env vars moose understands.

[env]
# --- macOS code signing ---
# MOOSE_SIGNING_IDENTITY           = "Developer ID Application: Your Name (TEAMID)"
# MOOSE_INSTALLER_SIGNING_IDENTITY = "Developer ID Installer: Your Name (TEAMID)"

# --- macOS notarization (set [macos.packaging].notarize = true in moose.toml) ---
# Either set up a keychain profile (preferred):
#     xcrun notarytool store-credentials MOOSE_NOTARY
# …or set explicit credentials here:
# APPLE_ID              = "you@example.com"
# TEAM_ID               = "ABCDEFG123"
# APP_SPECIFIC_PASSWORD = "xxxx-xxxx-xxxx-xxxx"

# --- Windows Authenticode signing ---
# Pick ONE of: Azure Trusted Signing, cert thumbprint, or .pfx file.
#
# Azure Trusted Signing:
# MOOSE_AZURE_ACCOUNT = "your-account"
# MOOSE_AZURE_PROFILE = "your-cert-profile"
# MOOSE_AZURE_DLIB    = 'C:\Program Files\Microsoft Trusted Signing Client\bin\x64\Azure.CodeSigning.Dlib.dll'
#
# Cert thumbprint (cert already in current user's store):
# MOOSE_CERT_SHA1  = "0123456789abcdef..."
# MOOSE_CERT_STORE = "My"
#
# .pfx file:
# MOOSE_PFX_PATH     = 'C:\path\to\cert.pfx'
# MOOSE_PFX_PASSWORD = "..."

# Optional override for the RFC 3161 timestamp server:
# MOOSE_TIMESTAMP_URL = "http://timestamp.digicert.com"
