use std::collections::HashMap;
use std::env;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VarFlags(u8);

impl VarFlags {
    pub const EXPORT: u8 = 0b0001;
    pub const READONLY: u8 = 0b0010;

    pub fn new() -> Self {
        VarFlags(0)
    }

    pub fn has(self, flag: u8) -> bool {
        self.0 & flag != 0
    }

    pub fn set(&mut self, flag: u8) {
        self.0 |= flag;
    }

    pub fn clear(&mut self, flag: u8) {
        self.0 &= !flag;
    }
}

/// A shell variable.
#[derive(Debug, Clone)]
pub struct Var {
    pub value: Option<String>,
    pub flags: VarFlags,
}

/// Saved variable state for scope restoration.
#[derive(Debug)]
struct SavedVar {
    name: String,
    previous: Option<Var>,
}

/// Variable scope pushed on function call or dot-script.
#[derive(Debug)]
pub struct Scope {
    saved: Vec<SavedVar>,
}

/// Variable storage with scoping support.
pub struct Variables {
    /// All variables (flat namespace, latest value wins).
    vars: HashMap<String, Var>,
    /// Stack of scopes for local variable restoration.
    scopes: Vec<Scope>,
    /// Positional parameters ($1, $2, ...).
    pub positional: Vec<String>,
    /// $0 — script name or shell name.
    pub arg0: String,
}

impl Default for Variables {
    fn default() -> Self {
        Self::new()
    }
}

impl Variables {
    pub fn new() -> Self {
        let mut vars = HashMap::new();

        // Import environment variables
        for (key, value) in env::vars() {
            vars.insert(
                key,
                Var {
                    value: Some(value),
                    flags: {
                        let mut f = VarFlags::new();
                        f.set(VarFlags::EXPORT);
                        f
                    },
                },
            );
        }

        // Set default IFS
        if !vars.contains_key("IFS") {
            vars.insert(
                "IFS".into(),
                Var {
                    value: Some(" \t\n".into()),
                    flags: VarFlags::new(),
                },
            );
        }

        Variables {
            vars,
            scopes: Vec::new(),
            positional: Vec::new(),
            arg0: "epsh".into(),
        }
    }

    /// Get a variable's value. Returns None if unset.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).and_then(|v| v.value.as_deref())
    }

    /// Set a variable. Returns Err if readonly.
    pub fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        if let Some(existing) = self.vars.get(name)
            && existing.flags.has(VarFlags::READONLY)
        {
            return Err(format!("{name}: readonly variable"));
        }

        let entry = self.vars.entry(name.to_string()).or_insert_with(|| Var {
            value: None,
            flags: VarFlags::new(),
        });
        entry.value = Some(value.to_string());

        // Sync to process environment if exported
        if entry.flags.has(VarFlags::EXPORT) {
            unsafe { env::set_var(name, value) };
        }

        Ok(())
    }

    /// Unset a variable. Returns Err if readonly.
    pub fn unset(&mut self, name: &str) -> Result<(), String> {
        if let Some(existing) = self.vars.get(name)
            && existing.flags.has(VarFlags::READONLY)
        {
            return Err(format!("{name}: readonly variable"));
        }
        self.vars.remove(name);
        unsafe { env::remove_var(name) };
        Ok(())
    }

    /// Mark a variable as exported.
    pub fn export(&mut self, name: &str) {
        let entry = self.vars.entry(name.to_string()).or_insert_with(|| Var {
            value: None,
            flags: VarFlags::new(),
        });
        entry.flags.set(VarFlags::EXPORT);
        if let Some(ref value) = entry.value {
            unsafe { env::set_var(name, value) };
        }
    }

    /// Mark a variable as readonly.
    pub fn set_readonly(&mut self, name: &str) {
        let entry = self.vars.entry(name.to_string()).or_insert_with(|| Var {
            value: None,
            flags: VarFlags::new(),
        });
        entry.flags.set(VarFlags::READONLY);
    }

    /// Push a new scope (for function calls).
    pub fn push_scope(&mut self) {
        self.scopes.push(Scope { saved: Vec::new() });
    }

    /// Pop the current scope, restoring all saved variables.
    pub fn pop_scope(&mut self) {
        if let Some(scope) = self.scopes.pop() {
            for saved in scope.saved.into_iter().rev() {
                match saved.previous {
                    Some(var) => {
                        self.vars.insert(saved.name, var);
                    }
                    None => {
                        self.vars.remove(&saved.name);
                    }
                }
            }
        }
    }

    /// Declare a local variable in the current scope. Saves the previous
    /// value for restoration when the scope is popped.
    pub fn make_local(&mut self, name: &str) {
        if let Some(scope) = self.scopes.last_mut() {
            let previous = self.vars.get(name).cloned();
            scope.saved.push(SavedVar {
                name: name.to_string(),
                previous,
            });
        }
    }

    /// Get a special parameter value ($?, $$, $#, $@, $*, $!, $-, $0, $1...).
    pub fn get_special(&self, name: &str, exit_status: i32, shell_pid: u32) -> Option<String> {
        match name {
            "?" => Some(exit_status.to_string()),
            "$" => Some(shell_pid.to_string()),
            "#" => Some(self.positional.len().to_string()),
            "0" => Some(self.arg0.clone()),
            "-" => Some(String::new()), // TODO: shell option flags
            "!" => None,                // TODO: last background pid
            "@" | "*" => {
                // These need special handling in expansion (IFS joining for *, separate fields for @)
                Some(self.positional.join(" "))
            }
            _ => {
                // Positional parameters $1, $2, ...
                if let Ok(n) = name.parse::<usize>() {
                    if n >= 1 {
                        self.positional.get(n - 1).cloned()
                    } else {
                        None
                    }
                } else {
                    self.get(name).map(String::from)
                }
            }
        }
    }

    /// Get the IFS value (defaults to " \t\n").
    pub fn ifs(&self) -> &str {
        self.get("IFS").unwrap_or(" \t\n")
    }

    /// Build the environment for execve: all exported variables.
    pub fn exported_env(&self) -> Vec<(String, String)> {
        self.vars
            .iter()
            .filter(|(_, v)| v.flags.has(VarFlags::EXPORT) && v.value.is_some())
            .map(|(k, v)| (k.clone(), v.value.clone().unwrap()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get() {
        let mut vars = Variables::new();
        vars.set("FOO", "bar").unwrap();
        assert_eq!(vars.get("FOO"), Some("bar"));
    }

    #[test]
    fn unset() {
        let mut vars = Variables::new();
        vars.set("FOO", "bar").unwrap();
        vars.unset("FOO").unwrap();
        assert_eq!(vars.get("FOO"), None);
    }

    #[test]
    fn readonly() {
        let mut vars = Variables::new();
        vars.set("FOO", "bar").unwrap();
        vars.set_readonly("FOO");
        assert!(vars.set("FOO", "baz").is_err());
        assert!(vars.unset("FOO").is_err());
    }

    #[test]
    fn scope_local() {
        let mut vars = Variables::new();
        vars.set("X", "outer").unwrap();
        vars.push_scope();
        vars.make_local("X");
        vars.set("X", "inner").unwrap();
        assert_eq!(vars.get("X"), Some("inner"));
        vars.pop_scope();
        assert_eq!(vars.get("X"), Some("outer"));
    }

    #[test]
    fn scope_new_local() {
        let mut vars = Variables::new();
        vars.push_scope();
        vars.make_local("Y");
        vars.set("Y", "local").unwrap();
        assert_eq!(vars.get("Y"), Some("local"));
        vars.pop_scope();
        assert_eq!(vars.get("Y"), None);
    }

    #[test]
    fn positional_params() {
        let mut vars = Variables::new();
        vars.positional = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(vars.get_special("#", 0, 1), Some("3".into()));
        assert_eq!(vars.get_special("1", 0, 1), Some("a".into()));
        assert_eq!(vars.get_special("3", 0, 1), Some("c".into()));
        assert_eq!(vars.get_special("4", 0, 1), None);
    }

    #[test]
    fn special_params() {
        let vars = Variables::new();
        assert_eq!(vars.get_special("?", 42, 1234), Some("42".into()));
        assert_eq!(vars.get_special("$", 0, 1234), Some("1234".into()));
    }

    #[test]
    fn ifs_default() {
        let vars = Variables::new();
        assert_eq!(vars.ifs(), " \t\n");
    }
}
