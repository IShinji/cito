# A directory entry prunes the whole subtree; entries are normalized
# relative to this conftest; globs use fnmatch, where `*` crosses `/`.
collect_ignore = [
    "ignored_dir",
    "./shadowed/../test_ignored_file.py",
    "shadowed/test_kept.py",
]
collect_ignore_glob = ["globbed/*_x.py"]
