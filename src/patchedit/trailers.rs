use clap::ArgMatches;

use crate::{commit::CommitMessage, error::Error, stupid};

pub(crate) fn add_trailers<'a>(
    message: CommitMessage<'a>,
    matches: &ArgMatches,
    signature: &git2::Signature,
    autosign: Option<&str>,
) -> Result<CommitMessage<'a>, Error> {
    let mut trailers: Vec<(&str, &str)> = vec![];

    let default_value = if let (Some(name), Some(email)) = (signature.name(), signature.email()) {
        format!("{} <{}>", name, email)
    } else {
        return Err(Error::NonUtf8Signature(
            "trailer requires utf-8 signature".to_string(),
        ));
    };

    for (opt_name, old_by_opt, trailer) in &[
        ("sign", "sign-by", "Signed-off-by"),
        ("ack", "ack-by", "Acked-by"),
        ("review", "review-by", "Reviewed-by"),
    ] {
        let mut values = if let Some(values) = matches.values_of(opt_name) {
            values.collect()
        } else {
            vec![]
        };

        // The option was provided at least once without a value.
        let occurrences: usize = (matches.occurrences_of(opt_name) as u64)
            .try_into()
            .unwrap();
        if values.len() < occurrences {
            values.push(default_value.as_str());
        }

        if let Some(by_values) = matches.values_of(old_by_opt) {
            values.extend(by_values);
        }

        for value in values {
            trailers.push((trailer, value));
        }
    }

    if let Some(autosign) = autosign {
        trailers.push((autosign, &default_value));
    }

    if trailers.is_empty() {
        Ok(message)
    } else {
        let message_str = message.decode()?;
        let message_bytes = stupid::interpret_trailers(message_str.as_bytes(), trailers)?;
        let message = String::from_utf8(message_bytes).map_err(|_| {
            Error::Generic("Could not decode message after adding trailers".to_string())
        })?;
        Ok(CommitMessage::from(message))
    }
}