use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
};

use nu_plugin::{EvaluatedCall, LabeledError};
use nu_protocol::{Span, Value};
use trust_dns_client::client::ClientHandle;
use trust_dns_proto::rr::{DNSClass, RecordType};
use trust_dns_resolver::{
    config::{Protocol, ResolverConfig},
    Name,
};

use self::{client::DnsClient, constants::flags, serde::RType};

mod client;
mod constants;
mod nu;
mod serde;

pub struct Dns {}

impl Dns {
    async fn run_impl(
        &mut self,
        name: &str,
        call: &EvaluatedCall,
        input: &Value,
    ) -> Result<Value, LabeledError> {
        match name {
            "dns query" => self.query(call, input).await,
            _ => Err(LabeledError {
                label: "NoSuchCommandError".into(),
                msg: "No such command".into(),
                span: Some(call.head),
            }),
        }
    }

    async fn query(&self, call: &EvaluatedCall, input: &Value) -> Result<Value, LabeledError> {
        let arg_inputs: Vec<Value> = call.rest(0)?;
        let input: Vec<&Value> = match input {
            Value::Nothing { .. } => arg_inputs.iter().collect(),
            val => {
                if !arg_inputs.is_empty() {
                    return Err(LabeledError {
                        label: "AmbiguousInputError".into(),
                        msg: "Input should either be positional args or piped, but not both".into(),
                        span: Some(val.span()?),
                    });
                }

                vec![val]
            }
        };

        let names = input
            .into_iter()
            .map(|input_name| match input_name {
                Value::String { val, span } => {
                    Ok(Name::from_utf8(val).map_err(|err| LabeledError {
                        label: "InvalidNameError".into(),
                        msg: format!("Error parsing name: {}", err),
                        span: Some(*span),
                    })?)
                }
                Value::List { vals, span } => Ok(Name::from_labels(
                    vals.iter()
                        .map(|val| {
                            if let Value::Binary { val: bin_val, .. } = val {
                                Ok(bin_val.clone())
                            } else {
                                Err(LabeledError {
                                    label: "InvalidNameError".into(),
                                    msg: "Invalid input type for name".into(),
                                    span: Some(val.span()?),
                                })
                            }
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                )
                .map_err(|err| LabeledError {
                    label: "NameParseError".into(),
                    msg: format!("Error parsing into name: {}", err),
                    span: Some(*span),
                })?),
                val => Err(LabeledError {
                    label: "InvalidInputTypeError".into(),
                    msg: "Invalid input type".into(),
                    span: Some(val.span()?),
                }),
            })
            .collect::<Result<Vec<_>, _>>()?;

        let protocol = match call.get_flag_value(flags::PROTOCOL) {
            None => None,
            Some(val) => Some(serde::Protocol::try_from(val).map(|serde::Protocol(proto)| proto)?),
        };

        let (addr, addr_span, protocol) = match call.get_flag_value(flags::SERVER) {
            Some(Value::String { val, span }) => {
                let addr = SocketAddr::from_str(&val)
                    .or_else(|_| {
                        IpAddr::from_str(&val)
                            .map(|ip| SocketAddr::new(ip, constants::config::SERVER_PORT))
                    })
                    .map_err(|err| LabeledError {
                        label: "InvalidServerAddress".into(),
                        msg: format!("Invalid server: {}", err),
                        span: Some(span),
                    })?;

                (addr, Some(span), protocol.unwrap_or(Protocol::Udp))
            }
            None => {
                let (config, _) =
                    trust_dns_resolver::system_conf::read_system_conf().unwrap_or_default();
                match config.name_servers() {
                    [ns, ..] => (ns.socket_addr, None, ns.protocol),
                    [] => {
                        let config = ResolverConfig::default();
                        let ns = config.name_servers().first().unwrap();

                        // if protocol is explicitly configured, it should take
                        // precedence over the system config
                        (ns.socket_addr, None, protocol.unwrap_or(ns.protocol))
                    }
                }
            }
            Some(val) => {
                return Err(LabeledError {
                    label: "InvalidServerAddressInputError".into(),
                    msg: "invalid input type for server address".into(),
                    span: Some(val.span()?),
                })
            }
        };

        let qtypes: Vec<RecordType> = match call.get_flag_value(flags::TYPE) {
            Some(Value::List { vals, .. }) => vals
                .into_iter()
                .map(RType::try_from)
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|RType(rtype)| rtype)
                .collect(),
            Some(val) => vec![RType::try_from(val)?.0],
            None => vec![RecordType::AAAA, RecordType::A],
        };

        let dns_class: DNSClass = match call.get_flag_value(flags::CLASS) {
            Some(val) => serde::DNSClass::try_from(val)?.0,
            None => DNSClass::IN,
        };

        let dnssec_mode = match call.get_flag_value(flags::DNSSEC) {
            Some(val) => serde::DnssecMode::try_from(val)?,
            None => serde::DnssecMode::Opportunistic,
        };

        let (mut client, _bg) = DnsClient::new(addr, addr_span, protocol, dnssec_mode).await?;

        let messages: Vec<_> = futures_util::future::join_all(names.into_iter().flat_map(|name| {
            qtypes
                .iter()
                .map(|qtype| client.query(name.clone(), dns_class, *qtype))
                .collect::<Vec<_>>()
        }))
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| LabeledError {
            label: "DNSResponseError".into(),
            msg: format!("Error in DNS response: {:?}", err),
            span: None,
        })?
        .into_iter()
        .map(|resp: trust_dns_proto::xfer::DnsResponse| {
            serde::Message(&resp.into_inner()).into_value(call)
        })
        .collect();

        let result = Value::record(
            vec![
                constants::columns::NAMESERVER.into(),
                constants::columns::MESSAGES.into(),
            ],
            vec![
                Value::record(
                    vec![
                        constants::columns::ADDRESS.into(),
                        constants::columns::PROTOCOL.into(),
                    ],
                    vec![
                        Value::string(addr.to_string(), Span::unknown()),
                        Value::string(protocol.to_string(), Span::unknown()),
                    ],
                    Span::unknown(),
                ),
                Value::list(messages, Span::unknown()),
            ],
            Span::unknown(),
        );

        Ok(result)
    }
}
