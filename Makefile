default:
	@grep '^[^#[:space:]].*:' Makefile

.NOTPARALLEL:

install:
	cargo install --locked --path .
	cp desktop/aw-fm.desktop "${HOME}/.local/share/applications/aw-fm.desktop"
	cp desktop/aw-fm-folder.desktop "${HOME}/.local/share/applications/aw-fm-folder.desktop"


uninstall:
	cargo uninstall aw-fm
	rm "${HOME}/.local/share/applications/aw-fm.desktop"
	rm "${HOME}/.local/share/applications/aw-fm-folder.desktop"

