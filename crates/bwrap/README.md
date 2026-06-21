# bwrap

Vendored fork of [Bwrap](https://github.com/micl2e2/bwrap/) for exgit:
- Updates to unicode-width 0.2
- Optimized performance

Bwrap is a fast, lightweight, embedded system-friendly library for
wrapping text. While bwrap offers great flexibility in wrapping text,
neither resource consumption nor performance compromises: 

1.  No heap allocation happens by default.

2.  The time/space complexity is *O(n)* by default, or *O(n(p+a))* if
    there is appending/prepending. (*n*, *p*, *a* is the number of
    input/prepending/appending bytes respectively) 

# License

bwrap can be licensed under either [MIT
License](https://github.com/micl2e2/bwrap/blob/master/LICENSE-MIT) **or**
[GNU General Public License Version
3.0](https://github.com/micl2e2/bwrap/blob/master/LICENSE-GPL). The
choice is **entirely** up to you. 
