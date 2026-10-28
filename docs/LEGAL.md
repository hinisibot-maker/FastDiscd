# Legal checklist before publishing

Please review the following before publishing this repository on GitHub.
**This is not legal advice. Consult a lawyer if necessary.**

## 1. Discord's software (copyright)

- This project contains **no Discord code**.
  It does not redistribute a modified Electron app; it simply displays `discord.com` in WebView2.
- Therefore it does not constitute copyright infringement (reproduction or adaptation) of Discord's client.
- However, Discord's **Terms of Service** prohibit unofficial clients.
  Users may be banned, and distributors may face ToS-related complaints. The risk is not zero.

## 2. Discord's name and logo (trademark)

- The name "Discord" and its logo are **trademarks of Discord Inc.**
  Using them in a product name or icon can be considered trademark infringement.
- If you publish, prefer one of the following:
  1. Use an original icon (recommended)
  2. Avoid "Discord" in the product name
  3. State clearly in the README that this is an unofficial project unrelated to Discord Inc.
- Including Discord's logo in the repository may lead to a DMCA / trademark takedown.

## 3. Dependency licenses

- Tauri / wry / tao: MIT / Apache-2.0
- webview2-com / windows-rs: MIT
- This project's code: MIT (see `LICENSE`)

All are redistributable. You can verify with `cargo deny` or `cargo license`.

## 4. Publishing recommendations

- Avoid "Discord" in the repository name, or clearly mark the project as unofficial
- Use an original icon
- Put a disclaimer at the top of the README
- Attach the built installer and source zip to Releases
- Include the license file and third-party notices
