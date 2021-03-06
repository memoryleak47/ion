use std::io::{self, Write};
use std::mem;
use super::status::*;
use super::Shell;
use super::flags::*;
use super::job_control::JobControl;
use super::flow_control::{ElseIf, Function, Statement, collect_loops, collect_cases, collect_if, Case};
use parser::{ForExpression, StatementSplitter, parse_and_validate, expand_string};
use parser::pipelines::Pipeline;
use shell::assignments::VariableStore;
use types::Array;

pub enum Condition {
    Continue,
    Break,
    NoOp,
    SigInt,
}

pub trait FlowLogic {
    /// Receives a command and attempts to execute the contents.
    fn on_command(&mut self, command_string: &str);

    /// The highest layer of the flow control handling which branches into lower blocks when found.
    fn execute_toplevel<I>(&mut self, iterator: &mut I, statement: Statement) -> Result<(), &'static str>
        where I: Iterator<Item = Statement>;

    /// Executes all of the statements within a while block until a certain condition is met.
    fn execute_while(&mut self, expression: Pipeline, statements: Vec<Statement>) -> Condition;

    /// Executes all of the statements within a for block for each value specified in the range.
    fn execute_for(&mut self, variable: &str, values: &[String], statements: Vec<Statement>) -> Condition;

    /// Conditionally executes branches of statements according to evaluated expressions
    fn execute_if(&mut self, expression: Pipeline, success: Vec<Statement>,
        else_if: Vec<ElseIf>, failure: Vec<Statement>) -> Condition;

    /// Simply executes all supplied statemnts.
    fn execute_statements(&mut self, statements: Vec<Statement>) -> Condition;

    /// Expand an expression and run a branch based on the value of the expanded expression
    fn execute_match(&mut self, expression: String, cases: Vec<Case>) -> Condition;

}

impl<'a> FlowLogic for Shell<'a> {
    fn on_command(&mut self, command_string: &str) {
        self.break_flow = false;
        let mut iterator = StatementSplitter::new(command_string).map(parse_and_validate);

        // If the value is set to `0`, this means that we don't need to append to an existing
        // partial statement block in memory, but can read and execute new statements.
        if self.flow_control.level == 0 {
            while let Some(statement) = iterator.next() {
                // Executes all statements that it can, and stores the last remaining partial
                // statement in memory if needed. We can tell if there is a partial statement
                // later if the value of `level` is not set to `0`.
                if let Err(why) = self.execute_toplevel(&mut iterator, statement) {
                    let stderr = io::stderr();
                    let mut stderr = stderr.lock();
                    let _ = writeln!(stderr, "{}", why);
                    self.flow_control.level = 0;
                    self.flow_control.current_if_mode = 0;
                    return
                }
            }
        } else {
            // Appends the newly parsed statements onto the existing statement stored in memory.
            match self.flow_control.current_statement {
                Statement::While{ ref mut statements, .. }
                    | Statement::For { ref mut statements, .. }
                    | Statement::Function { ref mut statements, .. } =>
                {
                    collect_loops(&mut iterator, statements, &mut self.flow_control.level);
                },
                Statement::If { ref mut success, ref mut else_if, ref mut failure, .. } => {
                    self.flow_control.current_if_mode = match collect_if(&mut iterator, success,
                        else_if, failure, &mut self.flow_control.level,
                        self.flow_control.current_if_mode) {
                            Ok(mode) => mode,
                            Err(why) => {
                                let stderr = io::stderr();
                                let mut stderr = stderr.lock();
                                let _ = writeln!(stderr, "{}", why);
                                4
                            }
                        };
                },
                Statement::Match { ref mut cases, .. } => {
                    if let Err(why) = collect_cases(&mut iterator, cases, &mut self.flow_control.level) {
                        let stderr = io::stderr();
                        let mut stderr = stderr.lock();
                        let _ = writeln!(stderr, "{}", why);
                    }
                },
                _ => ()
            }

            // If this is true, an error occurred during the if statement
            if self.flow_control.current_if_mode == 4 {
                self.flow_control.level = 0;
                self.flow_control.current_if_mode = 0;
                self.flow_control.current_statement = Statement::Default;
                return
            }

            // If the level is set to 0, it means that the statement in memory is finished
            // and thus is ready for execution.
            if self.flow_control.level == 0 {
                // Replaces the `current_statement` with a `Default` value to avoid the
                // need to clone the value, and clearing it at the same time.
                let mut replacement = Statement::Default;
                mem::swap(&mut self.flow_control.current_statement, &mut replacement);

                match replacement {
                    Statement::Error(number) => self.previous_status = number,
                    Statement::Let { expression } => {
                        self.previous_status = self.local(expression);
                    },
                    Statement::Export(expression) => {
                        self.previous_status = self.export(expression);
                    }
                    Statement::While { expression, statements } => {
                        if let Condition::SigInt = self.execute_while(expression, statements) {
                            return
                        }
                    },
                    Statement::For { variable, values, statements } => {
                        if let Condition::SigInt = self.execute_for(&variable, &values, statements) {
                            return
                        }
                    },
                    Statement::Function { name, args, statements, description } => {
                        self.functions.insert(name.clone(), Function {
                            name:       name,
                            args:       args,
                            statements: statements,
                            description: description,
                        });
                    },
                    Statement::If { expression, success, else_if, failure } => {
                        self.execute_if(expression, success, else_if, failure);
                    },
                    Statement::Match { expression, cases } => {
                        self.execute_match(expression, cases);
                    }
                    _ => ()
                }

                // Capture any leftover statements.
                while let Some(statement) = iterator.next() {
                    if let Err(why) = self.execute_toplevel(&mut iterator, statement) {
                        let stderr = io::stderr();
                        let mut stderr = stderr.lock();
                        let _ = writeln!(stderr, "{}", why);
                        self.flow_control.level = 0;
                        self.flow_control.current_if_mode = 0;
                        return
                    }
                }
            }
        }
    }

    fn execute_match(&mut self, expression: String, cases: Vec<Case>) -> Condition {
        // Logic for determining if the LHS of a match-case construct (the value we are matching
        // against) matches the RHS of a match-case construct (a value in a case statement). For
        // example, checking to see if the value "foo" matches the pattern "bar" would be invoked
        // like so :
        // ```ignore
        // matches("foo", "bar")
        // ```
        fn matches(lhs : &Array, rhs : &Array) -> bool {
            for v in lhs {
                if rhs.contains(&v) { return true; }
            }
            return false;
        }
        let value = expand_string(&expression, self, false);
        let mut condition = Condition::NoOp;
        for case in cases {
            let pattern = case.value.map(|v| { expand_string(&v, self, false) });
            match pattern {
                None => {
                    condition = self.execute_statements(case.statements);
                    break;
                }
                Some(ref v) if matches(v, &value) => {
                    condition = self.execute_statements(case.statements);
                    break;
                }
                Some(_) => (),
            }
        }
        condition
    }

    fn execute_statements(&mut self, mut statements: Vec<Statement>) -> Condition {
        let mut iterator = statements.drain(..);
        while let Some(statement) = iterator.next() {
            match statement {
                Statement::Error(number) => self.previous_status = number,
                Statement::Let { expression } => {
                    self.previous_status = self.local(expression);
                },
                Statement::Export(expression) => {
                    self.previous_status = self.export(expression);
                }
                Statement::While { expression, mut statements } => {
                    self.flow_control.level += 1;
                    collect_loops(&mut iterator, &mut statements, &mut self.flow_control.level);
                    if let Condition::SigInt = self.execute_while(expression, statements) {
                        return Condition::SigInt;
                    }
                },
                Statement::For { variable, values, mut statements } => {
                    self.flow_control.level += 1;
                    collect_loops(&mut iterator, &mut statements, &mut self.flow_control.level);
                    if let Condition::SigInt = self.execute_for(&variable, &values, statements) {
                        return Condition::SigInt;
                    }
                },
                Statement::If { expression, mut success, mut else_if, mut failure } => {
                    self.flow_control.level += 1;
                    if let Err(why) = collect_if(&mut iterator, &mut success, &mut else_if,
                        &mut failure, &mut self.flow_control.level, 0)
                    {
                        let stderr = io::stderr();
                        let mut stderr = stderr.lock();
                        let _ = writeln!(stderr, "{}", why);
                        self.flow_control.level = 0;
                        self.flow_control.current_if_mode = 0;
                        return Condition::Break
                    }

                    match self.execute_if(expression, success, else_if, failure) {
                        Condition::Break    => return Condition::Break,
                        Condition::Continue => return Condition::Continue,
                        Condition::NoOp     => (),
                        Condition::SigInt   => return Condition::SigInt,
                    }
                },
                Statement::Function { name, args, mut statements, description } => {
                    self.flow_control.level += 1;
                    collect_loops(&mut iterator, &mut statements, &mut self.flow_control.level);
                    self.functions.insert(name.clone(), Function {
                        description: description,
                        name:        name,
                        args:        args,
                        statements:  statements
                    });
                },
                Statement::Pipeline(mut pipeline)  => {
                    self.run_pipeline(&mut pipeline);
                    if self.flags & ERR_EXIT != 0 && self.previous_status != SUCCESS {
                        let status = self.previous_status;
                        self.exit(status);
                    }
                },
                Statement::Break => { return Condition::Break }
                Statement::Continue => { return Condition::Continue }
                Statement::Match {expression, mut cases} => {
                    self.flow_control.level += 1;
                    if let Err(why) = collect_cases(&mut iterator, &mut cases, &mut self.flow_control.level) {
                        let stderr = io::stderr();
                        let mut stderr = stderr.lock();
                        let _ = writeln!(stderr, "{}", why);
                        self.flow_control.level = 0;
                        self.flow_control.current_if_mode = 0;
                        return Condition::Break
                    }
                    match self.execute_match(expression, cases) {
                        Condition::Break    => return Condition::Break,
                        Condition::Continue => return Condition::Continue,
                        Condition::NoOp     => (),
                        Condition::SigInt   => return Condition::SigInt,
                    }
                }
                _ => {}
            }
            if let Some(signal) = self.next_signal() {
                if self.handle_signal(signal) {
                    self.exit(get_signal_code(signal));
                }
                return Condition::SigInt;
            } else if self.break_flow {
                self.break_flow = false;
                return Condition::SigInt;
            }
        }
        Condition::NoOp
    }

    fn execute_while (
        &mut self,
        expression: Pipeline,
        statements: Vec<Statement>
    ) -> Condition {
        while self.run_pipeline(&mut expression.clone()) == Some(SUCCESS) {
            // Cloning is needed so the statement can be re-iterated again if needed.
            match self.execute_statements(statements.clone()) {
                Condition::Break  => break,
                Condition::SigInt => return Condition::SigInt,
                _                 => ()
            }
        }
        Condition::NoOp
    }

    fn execute_for (
        &mut self,
        variable: &str,
        values: &[String],
        statements: Vec<Statement>
    ) -> Condition {
        let ignore_variable = variable == "_";
        match ForExpression::new(values, self) {
            ForExpression::Multiple(ref values) if ignore_variable => {
                for _ in values.iter() {
                    match self.execute_statements(statements.clone()) {
                        Condition::Break  => break,
                        Condition::SigInt => return Condition::SigInt,
                        _                 => ()
                    }
                }
            },
            ForExpression::Multiple(values) => {
                for value in values.iter() {
                    self.variables.set_var(variable, &value);
                    match self.execute_statements(statements.clone()) {
                        Condition::Break  => break,
                        Condition::SigInt => return Condition::SigInt,
                        _                 => ()
                    }
                }
            },
            ForExpression::Normal(ref values) if ignore_variable => {
                for _ in values.lines() {
                    match self.execute_statements(statements.clone()) {
                        Condition::Break  => break,
                        Condition::SigInt => return Condition::SigInt,
                        _                 => ()
                    }
                }
            },
            ForExpression::Normal(values) => {
                for value in values.lines() {
                    self.variables.set_var(variable, &value);
                    match self.execute_statements(statements.clone()) {
                        Condition::Break  => break,
                        Condition::SigInt => return Condition::SigInt,
                        _                 => ()
                    }
                }
            },
            ForExpression::Range(start, end) if ignore_variable => {
                for _ in start..end {
                    match self.execute_statements(statements.clone()) {
                        Condition::Break  => break,
                        Condition::SigInt => return Condition::SigInt,
                        _                 => ()
                    }
                }
            }
            ForExpression::Range(start, end) => {
                for value in (start..end).map(|x| x.to_string()) {
                    self.variables.set_var(variable, &value);
                    match self.execute_statements(statements.clone()) {
                        Condition::Break  => break,
                        Condition::SigInt => return Condition::SigInt,
                        _                 => ()
                    }
                }
            }
        }
        Condition::NoOp
    }

    fn execute_if(&mut self, mut expression: Pipeline, success: Vec<Statement>,
        else_if: Vec<ElseIf>, failure: Vec<Statement>) -> Condition
    {
        match self.run_pipeline(&mut expression) {
            Some(SUCCESS) => self.execute_statements(success),
            _             => {
                for mut elseif in else_if {
                    if self.run_pipeline(&mut elseif.expression) == Some(SUCCESS) {
                        return self.execute_statements(elseif.success);
                    }
                }
                self.execute_statements(failure)
            }
        }
    }

    fn execute_toplevel<I>(&mut self, iterator: &mut I, statement: Statement) -> Result<(), &'static str>
        where I: Iterator<Item = Statement>
    {
        match statement {
            Statement::Error(number) => self.previous_status = number,
            // Execute a Let Statement
            Statement::Let { expression } => {
                self.previous_status = self.local(expression);
            },
            Statement::Export(expression) => {
               self.previous_status = self.export(expression);
            }
            // Collect the statements for the while loop, and if the loop is complete,
            // execute the while loop with the provided expression.
            Statement::While { expression, mut statements } => {
                self.flow_control.level += 1;

                // Collect all of the statements contained within the while block.
                collect_loops(iterator, &mut statements, &mut self.flow_control.level);

                if self.flow_control.level == 0 {
                    // All blocks were read, thus we can immediately execute now
                    self.execute_while(expression, statements);
                } else {
                    // Store the partial `Statement::While` to memory
                    self.flow_control.current_statement = Statement::While {
                        expression: expression,
                        statements: statements,
                    }
                }
            },
            // Collect the statements for the for loop, and if the loop is complete,
            // execute the for loop with the provided expression.
            Statement::For { variable, values, mut statements } => {
                self.flow_control.level += 1;

                // Collect all of the statements contained within the for block.
                collect_loops(iterator, &mut statements, &mut self.flow_control.level);

                if self.flow_control.level == 0 {
                    // All blocks were read, thus we can immediately execute now
                    self.execute_for(&variable, &values, statements);
                } else {
                    // Store the partial `Statement::For` to memory
                    self.flow_control.current_statement = Statement::For {
                        variable:   variable,
                        values:     values,
                        statements: statements,
                    }
                }
            },
            // Collect the statements needed for the `success`, `else_if`, and `failure`
            // conditions; then execute the if statement if it is complete.
            Statement::If { expression, mut success, mut else_if, mut failure } => {
                self.flow_control.level += 1;

                // Collect all of the success and failure statements within the if condition.
                // The `mode` value will let us know whether the collector ended while
                // collecting the success block or the failure block.
                let mode = collect_if(iterator, &mut success, &mut else_if,
                    &mut failure, &mut self.flow_control.level, 0)?;

                if self.flow_control.level == 0 {
                    // All blocks were read, thus we can immediately execute now
                    self.execute_if(expression, success, else_if, failure);
                } else {
                    // Set the mode and partial if statement in memory.
                    self.flow_control.current_if_mode = mode;
                    self.flow_control.current_statement = Statement::If {
                        expression: expression,
                        success:    success,
                        else_if:    else_if,
                        failure:    failure
                    };
                }
            },
            // Collect the statements needed by the function and add the function to the
            // list of functions if it is complete.
            Statement::Function { name, args, mut statements, description } => {
                self.flow_control.level += 1;

                // The same logic that applies to loops, also applies here.
                collect_loops(iterator, &mut statements, &mut self.flow_control.level);

                if self.flow_control.level == 0 {
                    // All blocks were read, thus we can add it to the list
                    self.functions.insert(name.clone(), Function {
                        description: description,
                        name:        name,
                        args:        args,
                        statements:  statements
                    });
                } else {
                    // Store the partial function declaration in memory.
                    self.flow_control.current_statement = Statement::Function {
                        description: description,
                        name:        name,
                        args:        args,
                        statements:  statements
                    }
                }
            },
            // Simply executes a provided pipeline, immediately.
            Statement::Pipeline(mut pipeline)  => {
                self.run_pipeline(&mut pipeline);
                if self.flags & ERR_EXIT != 0 && self.previous_status != SUCCESS {
                    let status = self.previous_status;
                    self.exit(status);
                }
            },
            // At this level, else and else if keywords are forbidden.
            Statement::ElseIf{..} | Statement::Else => {
                let stderr = io::stderr();
                let mut stderr = stderr.lock();
                let _ = writeln!(stderr, "ion: syntax error: not an if statement");
            },
            // Likewise to else and else if, the end keyword does nothing here.
            Statement::End => {
                let stderr = io::stderr();
                let mut stderr = stderr.lock();
                let _ = writeln!(stderr, "ion: syntax error: no block to end");
            },
            // Collect all cases that are being used by a match construct
            Statement::Match {expression, mut cases} => {
                self.flow_control.level += 1;
                if let Err(why) = collect_cases(iterator, &mut cases, &mut self.flow_control.level) {
                    let stderr = io::stderr();
                    let mut stderr = stderr.lock();
                    let _ = writeln!(stderr, "{}", why);
                }
                if self.flow_control.level == 0 {
                    // If all blocks were read we execute the statement
                    self.execute_match(expression, cases);
                } else {
                    // Store the partial function declaration in memory.
                    self.flow_control.current_statement = Statement::Match {expression, cases};
                }
            }
            _ => {}
        }
        Ok(())
    }
}
