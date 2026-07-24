# Arch Linux packages

The client and server have separate VCS packages:

- `chatt-git` installs `/usr/bin/chatt`.
- `chatt-server-git` installs `/usr/bin/chatt-server`.

Each PKGBUILD uses the root of the containing Git checkout as its VCS source.
Run `makepkg` from the package directory:

```sh
cd packaging/arch/chatt-git
makepkg -si
```

Replace `chatt-git` with `chatt-server-git` to build the server package.
Because makepkg clones the local repository, the package contains the current
committed revision; uncommitted working-tree changes are not included.
