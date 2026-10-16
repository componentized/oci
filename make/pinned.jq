# generates target/tools/pinned.mk from `cargo metadata --no-deps` of tools/Cargo.toml, see make/tools.mk
.packages[0] as $tools
| ($tools.dependencies[].name | select($tools.metadata.bins[.] == null) | error("\(.) is missing from [package.metadata.bins] in tools/Cargo.toml")),
  "TOOLS := \([$tools.dependencies[].name] | join(" "))",
  "TOOLS_PINNED := \([$tools.dependencies[] | "\(.name)@\(.req | ltrimstr("="))"] | join(" "))",
  ($tools.metadata.bins[][] | "\(ascii_upcase | gsub("-"; "_")) := $(TOOLS_BIN)/\(.)")
