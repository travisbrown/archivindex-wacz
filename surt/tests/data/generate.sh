#!/bin/sh
# Regenerate python-surt.tsv and warcio.tsv from urls.txt with the reference implementations.
# Requires the Python `surt` package (`pip install surt`) and warcio.js (`npm install warcio`);
# rows omitted from each fixture are described in its header and must be filtered by hand.
set -e
cd "$(dirname "$0")"
python3 -c '
import sys, surt
for line in open("urls.txt"):
    url = line.rstrip("\n")
    try:
        key = surt.surt(url)
    except Exception as error:
        key = "!error " + type(error).__name__
    print(url + "\t" + key)
' > python-surt.raw.tsv
node --input-type=module -e '
import { getSurt } from "warcio";
import { readFileSync } from "node:fs";
for (const url of readFileSync("urls.txt", "utf8").split("\n").slice(0, -1)) {
  console.log(url + "\t" + getSurt(url));
}
' > warcio.raw.tsv
