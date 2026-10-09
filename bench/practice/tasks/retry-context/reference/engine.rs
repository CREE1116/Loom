#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelError {
    Transient,
    Auth,
}

#[derive(Clone, Debug)]
pub enum Command {
    Read {
        path: String,
    },
    Replace {
        path: String,
        old: String,
        new: String,
    },
    Finish {
        summary: String,
    },
}
pub trait Model {
    fn next(&mut self, context: &[String]) -> Result<Command, ModelError>;
}
pub trait Files {
    fn read(&mut self, path: &str) -> Result<String, String>;
    fn replace(&mut self, path: &str, old: &str, new: &str) -> Result<(), String>;
}
#[derive(Debug, PartialEq, Eq)]
pub enum RunError {
    Model(ModelError),
    Tool(String),
    Limit,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    pub summary: String,
    pub model_calls: usize,
    pub tool_calls: usize,
}

pub fn run(
    model: &mut impl Model,
    files: &mut impl Files,
    max_calls: usize,
) -> Result<Report, RunError> {
    let mut context = Vec::new();
    let mut tool_calls = 0;
    let mut failures = 0;
    for call in 1..=max_calls {
        let command = match model.next(&context) {
            Ok(command) => {
                failures = 0;
                command
            }
            Err(ModelError::Transient) => {
                failures += 1;
                if failures >= 3 {
                    return Err(RunError::Model(ModelError::Transient));
                }
                continue;
            }
            Err(error) => return Err(RunError::Model(error)),
        };
        let result = match command {
            Command::Read { path } => {
                tool_calls += 1;
                files.read(&path).map(|text| format!("READ {path}: {text}"))
            }
            Command::Replace { path, old, new } => {
                tool_calls += 1;
                files
                    .replace(&path, &old, &new)
                    .map(|()| format!("EDIT {path}: done"))
            }
            Command::Finish { summary } => {
                return Ok(Report {
                    summary,
                    model_calls: call,
                    tool_calls,
                })
            }
        };
        context.push(result.unwrap_or_else(|error| format!("ERROR: {error}")));
    }
    Err(RunError::Limit)
}
