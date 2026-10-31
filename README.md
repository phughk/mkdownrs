# mkdownrs

Point at a file, and it will create an http service serving renders of markdown files.

The program in memory hashes all mkdown paths it encounters, and can retrieve them again on lookup

```
cd some_dir
mkdownrs README.md   # serve one file
mkdownrs             # index every markdown file under the current directory
mkdownrs docs/       # index every markdown file under docs/
mkdownrs docs/ -s    # strict: only the most sensible markdown file (README/index, else first) and what it links to
```

Given a directory (or nothing), all markdown files in it are in scope. `-s` / `--strict` drops that and starts from the single most sensible file, following links from there.

The above will bind tcp 0 on localhost and offer some url, say `http://localhost:9456/hashhashhash123123` would point to that README.md, where the hash is from the full file path of README.md.

Going to the route `http://localhost:9456` redirects to that hash page.
Every relative or absolute path gets the same treatment.

The hashing is used as a lookup as well, if it's been indexed, the full path is resolved from the lookup and served.

Renders should be done with minijinja.
There should be a table of contents on the left side of each page, and rendered content in the primary div.

## Binding

`-b` / `--bind` picks where to listen (default: localhost, random port).

```
mkdownrs --bind :8080          # localhost:8080
mkdownrs --bind 192.168.1.5    # that address, random port
mkdownrs --bind 0.0.0.0:8080   # all interfaces; a link is printed for each local address
```

## Standalone export

```
mkdownrs --export out.html            # every markdown file under the current directory
mkdownrs README.md --export out.html  # README.md plus everything reachable by links below its directory
```

Writes a single self-contained HTML file instead of serving: all documents are rendered into it, links between them become `#/{hash}` routes, linked images/files are inlined as `data:` URIs, and mermaid is bundled (`assets/mermaid.min.js`, mermaid 11), so it works offline. Only files under the current directory (or under the given file's directory) are bundled; links pointing outside it are left as they were.
