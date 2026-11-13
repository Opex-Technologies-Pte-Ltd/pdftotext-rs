#!/usr/bin/env bash
# Download the public manual corpus listed in corpus/manuals.tsv into
# corpus/pdfs/ and verify each file's sha256. Vendors move files; a URL that
# fails or a checksum that no longer matches is reported, not fatal.
#
#   scripts/fetch-corpus.sh            # all
#   scripts/fetch-corpus.sh skf-bearing atlas-cam
set -uo pipefail
cd "$(dirname "$0")/.."
mkdir -p corpus/pdfs
ok=0; bad=0
while IFS=$'\t' read -r name url sha bytes pages desc; do
  [[ -z "$name" || "$name" == \#* || "$name" == name ]] && continue
  if (($#)) && [[ " $* " != *" $name "* ]]; then continue; fi
  out="corpus/pdfs/$name.pdf"
  if [[ ! -s "$out" ]] && ! curl -sSfL -A "Mozilla/5.0" --max-time 300 -o "$out.part" "$url"; then
    echo "FAIL  $name: download failed ($url)"; rm -f "$out.part"; bad=$((bad+1)); continue
  fi
  [[ -f "$out.part" ]] && mv "$out.part" "$out"
  got=$(sha256sum "$out" | cut -c1-64)
  if [[ "$got" == "$sha" ]]; then echo "ok    $name ($pages pages)"; ok=$((ok+1))
  else echo "DIFF  $name: checksum $got (expected $sha); the vendor may have updated it"; bad=$((bad+1)); fi
done < corpus/manuals.tsv
echo "$ok verified, $bad failed or changed"
echo "Check: PDFTOTEXT_CORPUS_DIR=corpus/pdfs cargo test --test parity -- --nocapture"
