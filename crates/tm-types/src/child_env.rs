//! The environment a command the agent runs on someone's behalf should see.
//!
//! Such a command has to work in the user's real project: their `PATH` (toolchain managers,
//! Homebrew), `HOME`, locale and terminal settings. What it must not see are the credentials
//! Ticketmaster itself is running with, so a model can't `env` or `printenv` its way to the keys
//! it is being served with. [`inherited_child_env`] is exactly that: this process's environment
//! minus [`CREDENTIAL_ENV_VARS`].

/// Credential variables Ticketmaster itself reads: model-provider API keys and the tracker tokens
/// `tm mirror` uses. Never passed to a command the agent runs.
pub const CREDENTIAL_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "AWS_SESSION_TOKEN",
    "AZURE_OPENAI_API_KEY",
    "CEREBRAS_API_KEY",
    "CF_API_TOKEN",
    "DEEPSEEK_API_KEY",
    "DEVPASS_API_KEY",
    "FIREWORKS_API_KEY",
    "GEMINI_API_KEY",
    "GITHUB_MODELS_TOKEN",
    "GITHUB_TOKEN",
    "GITLAB_TOKEN",
    "GOOGLE_API_KEY",
    "GROQ_API_KEY",
    "HF_TOKEN",
    "JIRA_API_TOKEN",
    "LINEAR_API_KEY",
    "LLAMA_CPP_API_KEY",
    "LLM_GATEWAY_API_KEY",
    "MISTRAL_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "TOGETHER_API_KEY",
    "VERTEX_ACCESS_TOKEN",
    "XAI_API_KEY",
];

/// Whether `name` is one of [`CREDENTIAL_ENV_VARS`].
pub fn is_credential_env_var(name: &str) -> bool {
    CREDENTIAL_ENV_VARS.contains(&name)
}

/// This process's environment minus [`CREDENTIAL_ENV_VARS`], as `(name, value)` pairs.
pub fn inherited_child_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(name, _)| !is_credential_env_var(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_recognized_and_ordinary_variables_are_not() {
        assert!(is_credential_env_var("DEVPASS_API_KEY"));
        assert!(is_credential_env_var("OPENAI_API_KEY"));
        assert!(!is_credential_env_var("PATH"));
        assert!(!is_credential_env_var("HOME"));
    }

    #[test]
    fn the_list_is_sorted_and_unique_so_additions_stay_reviewable() {
        let mut sorted = CREDENTIAL_ENV_VARS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, CREDENTIAL_ENV_VARS);
    }
}
