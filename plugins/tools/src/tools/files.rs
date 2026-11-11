//! Reading a file in the project.
//!
//! One line long, because the hard part lives in `workspace`, which is the
//! capability boundary it reads through. Changing a file is `patch`; listing a
//! directory is the shell's.

use super::{schema::ReadArgs, workspace::Workspace};
use crate::Result;

/// Lines of a text file, from `offset`, at most `limit` of them.
pub(super) fn read(workspace: &mut Workspace, args: &ReadArgs) -> Result<String> {
    let (_, text) = workspace.read_lines(&args.path, args.offset.unwrap_or(0), args.limit.unwrap_or(2000))?;
    Ok(text)
}
