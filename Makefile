.PHONY: gen core core-debug app run test release icons clean

APP := build/Build/Products/Release/Tulma.app
ASSETS := Tulma/Resources/Assets.xcassets

gen:
	xcodegen generate

core:
	cd core && cargo build --release -p tulma

core-debug:
	cd core && cargo build -p tulma -p tulma-core --bins

app: core gen
	xcodebuild -project Tulma.xcodeproj -scheme Tulma -configuration Release \
		-derivedDataPath build -destination 'platform=macOS,arch=arm64' build -quiet

run: app
	open $(APP)

test: core-debug gen
	cd core && cargo test
	xcodebuild -project Tulma.xcodeproj -scheme Tulma -derivedDataPath build \
		-destination 'platform=macOS,arch=arm64' test -quiet

# Developer ID signed, notarized DMG in build/release; see docs/releasing.md
release:
	scripts/release.sh

# Needs librsvg: brew install librsvg
icons:
	for s in 16 32 128 256 512; do \
		rsvg-convert -w $$s -h $$s docs/icon/app-icon.svg -o $(ASSETS)/AppIcon.appiconset/icon_$${s}x$${s}.png; \
		rsvg-convert -w $$((s * 2)) -h $$((s * 2)) docs/icon/app-icon.svg -o $(ASSETS)/AppIcon.appiconset/icon_$${s}x$${s}@2x.png; \
	done
	rsvg-convert -f pdf docs/icon/menubar-icon.svg -o $(ASSETS)/MenuBarIcon.imageset/MenuBarIcon.pdf
	rsvg-convert -f pdf docs/icon/menubar-icon-error.svg -o $(ASSETS)/MenuBarIconError.imageset/MenuBarIconError.pdf
	rsvg-convert -w 256 -h 256 docs/icon/app-icon.svg -o docs/images/icon.png

clean:
	rm -rf build Tulma.xcodeproj
	cd core && cargo clean
