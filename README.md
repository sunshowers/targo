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
