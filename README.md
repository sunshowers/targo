# targo

Wraps cargo to move target directories to a central location [super experimental]

To use,

```
cargo install --git https://github.com/sunshowers/targo --bin targo
```

Then, add this to your .zshrc/.bash_profile:

```
alias cargo='targo wrap-cargo'
```

## About

See [this comment on rust-lang/cargo](https://github.com/rust-lang/cargo/issues/11156#issuecomment-1285951209) for the execution model and considerations as of 2022-10-22.

## Looking for co-maintainers

This MVP works for myself and I only plan to add features as I need them. If you're a Rust developer who cares about this issue, and would like to help drive this project forward, please reach out to me at the email I use for my git commits with:
* some information about yourself
* why you're interested
* where you'd like to take this project

There's plenty to do:

* write tests
* add target directory management and garbage collection
* add configuration options
* stay up-to-date with upstream Cargo features
