# mkdownrs

Point at a file, and it will create an http service serving renders of markdown files.

The program in memory hashes all mkdown paths it encounters, and can retrieve them again on lookup

```
cd some_dir
mkdownrs README.md   # serve one file
mkdownrs             # index every markdown file under the current directory
```

The above will bind tcp 0 on localhost and offer some url, say `http://localhost:9456/hashhashhash123123` would point to that README.md, where the hash is from the full file path of README.md.

Going to the route `http://localhost:9456` redirects to that hash page.
Every relative or absolute path gets the same treatment.

The hashing is used as a lookup as well, if it's been indexed, the full path is resolved from the lookup and served.

Renders should be done with minijinja.
There should be a table of contents on the left side of each page, and rendered content in the primary div.
