#!/bin/bash

find -name '*.qml' | \
xgettext \
	--from-code=UTF-8 \
	--language=javascript \
	-o po/deb-installer.pot \
	--keyword=i18n:1 \
	--keyword=i18nc:1c,2 \
	--keyword=i18np:1,2 \
	--keyword=i18ncp:1c,2,3 \
	-f -

for i in *.po; do
	msgmerge \
		-U \
		--backup=none \
		$i deb-installer.pot
done
