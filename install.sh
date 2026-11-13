#!/bin/sh
# Builds Super Simple and installs the `supersimple` command so it can be run
# from any terminal. Run again after changing the code to update it.
set -e
cd "$(dirname "$0")"

if ! command -v cargo >/dev/null 2>&1; then
  echo "Rust is not installed. Install it with:  brew install rust"
  exit 1
fi

echo "Building…"
cargo build --release

# Pick a folder that is already on the PATH and that we can write to.
on_path() { case ":$PATH:" in *":$1:"*) return 0 ;; *) return 1 ;; esac; }
DEST=""
for dir in "$HOME/.local/bin" "/opt/homebrew/bin" "/usr/local/bin" "$HOME/.cargo/bin"; do
  if on_path "$dir" && [ -d "$dir" ] && [ -w "$dir" ]; then
    DEST="$dir"
    break
  fi
done

if [ -z "$DEST" ]; then
  # Nothing suitable on the PATH: use ~/.local/bin and add it to the zsh profile.
  DEST="$HOME/.local/bin"
  mkdir -p "$DEST"
  LINE='export PATH="$HOME/.local/bin:$PATH"'
  if ! grep -qsF "$LINE" "$HOME/.zshrc"; then
    printf '\n# Added by the Super Simple installer\n%s\n' "$LINE" >> "$HOME/.zshrc"
    echo "Added $DEST to your PATH in ~/.zshrc - open a new terminal window for it to take effect."
  fi
fi

install -m 755 target/release/supersimple "$DEST/supersimple"
echo "Installed: $DEST/supersimple"
echo "Run it with:  supersimple"
