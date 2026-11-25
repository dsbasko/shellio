//! `shellioctl` — CLI управления shellio: тонкая оболочка над клиентом `shellio-control`
//! (ADR-0007). Разбор аргументов, вызов, вывод JSON или текста, код выхода.

#![forbid(unsafe_code)]

mod args;
mod output;

use std::process::ExitCode;

use shellio_control::{
    Address, AddressError, Client, ClientError, Exit, KeymapRequest, RequestBody, Response,
    SOCKET_VAR, Subscription, TokioClock,
};
use shellio_core::Query;
use shellio_store::Profile;

use crate::args::{Action, Invocation, Parsed};
use crate::output::Printer;

const CLIENT: &str = concat!("shellioctl ", env!("CARGO_PKG_VERSION"));

fn main() -> ExitCode {
    ExitCode::from(run().code())
}

fn run() -> Exit {
    let absolute = |path: &std::path::Path| std::path::absolute(path);
    let invocation = match args::parse(std::env::args_os().skip(1).collect(), &absolute) {
        Ok(Parsed::Help) => return output::help(),
        Ok(Parsed::Run(invocation)) => invocation,
        Err(usage) => return output::usage_error(&usage),
    };
    let Invocation {
        json,
        timeout,
        socket,
        action,
    } = invocation;
    let address = match Address::resolve(
        socket,
        std::env::var_os(SOCKET_VAR),
        Profile::from_process_env,
    ) {
        Ok(address) => address,
        Err(error @ AddressError::EmptySocket { .. }) => {
            return output::usage_error(&args::Usage(error.to_string()));
        }
        Err(error @ AddressError::Profile(_)) => return output::failure(error),
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return output::failure(format_args!("runtime не запущен: {error}")),
    };
    let client = Client::new(address, TokioClock, CLIENT, timeout);
    runtime.block_on(execute(&client, action, Printer::new(json)))
}

async fn execute(client: &Client<TokioClock>, action: Action, printer: Printer) -> Exit {
    let body = match action {
        Action::Version => return finish(printer, client.version().await),
        Action::Follow => return follow(client.subscribe().await, printer).await,
        Action::Keymap(KeymapRequest::Subscribe) => {
            return follow(client.subscribe_keymap().await, printer).await;
        }
        Action::Keymap(request) => RequestBody::Keymap(request),
        Action::Status => RequestBody::Query(Query::Status {}),
        Action::Tree(scope) => RequestBody::Query(Query::Tree { scope }),
        Action::Layout { scope, workbench } => {
            RequestBody::Query(Query::Layout { scope, workbench })
        }
        Action::Panel { scope, panel } => RequestBody::Query(Query::Panel { scope, panel }),
        Action::Plugins => RequestBody::Query(Query::Plugins {}),
        Action::Command { command, scope } => RequestBody::Command { command, scope },
        Action::EventsSince(cursor) => RequestBody::EventsSince { cursor },
        Action::Ui(request) => RequestBody::Ui(request),
    };
    finish(printer, client.call(body).await)
}

fn finish(printer: Printer, outcome: Result<Response, ClientError>) -> Exit {
    match outcome {
        Ok(frame) => printer.frame(&frame),
        Err(error) => printer.client_error(&error),
    }
}

/// Поток подписки до последнего кадра: отказа ядра или транспорта, в том числе
/// `owner_leaving`, либо обрыва.
async fn follow(opened: Result<Subscription, ClientError>, printer: Printer) -> Exit {
    let mut subscription = match opened {
        Ok(subscription) => subscription,
        Err(error) => return printer.client_error(&error),
    };
    loop {
        let frame = match subscription.next().await {
            Ok(frame) => frame,
            Err(error) => return printer.client_error(&error),
        };
        let streaming = match &frame {
            Response::Snapshot { .. }
            | Response::Event { .. }
            | Response::Reset { .. }
            | Response::Keymap { .. } => true,
            Response::Ok { .. } | Response::CoreError { .. } | Response::Error { .. } => false,
        };
        let exit = printer.frame(&frame);
        if !streaming || exit != Exit::Success {
            return exit;
        }
    }
}
