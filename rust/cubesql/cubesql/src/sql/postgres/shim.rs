use std::{
    collections::HashMap,
    io::{Error, ErrorKind},
    sync::Arc,
};

use datafusion::{dataframe::DataFrame, execution::dataframe_impl::DataFrameImpl};
use log::{debug, error, trace};
use tokio::{io::AsyncWriteExt, net::TcpStream};

use crate::{
    compile::{
        convert_sql_to_cube_query, convert_statement_to_cube_query, parser::parse_sql_to_statement,
        QueryPlan,
    },
    sql::{
        dataframe::{batch_to_dataframe, TableValue},
        session::DatabaseProtocol,
        AuthContext, PgType, PgTypeId, QueryResponse, Session,
    },
    CubeError,
};

use super::{
    buffer,
    protocol::{self, FrontendMessage, SSL_REQUEST_PROTOCOL},
    statement::PreparedStatement,
};

pub struct Portal {
    plan: QueryPlan,
}

impl Portal {
    async fn fetch(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

pub struct AsyncPostgresShim {
    socket: TcpStream,
    #[allow(unused)]
    parameters: HashMap<String, String>,
    statements: HashMap<String, PreparedStatement>,
    portals: HashMap<String, Portal>,
    // Shared
    session: Arc<Session>,
}

#[derive(PartialEq, Eq)]
pub enum StartupState {
    Success,
    SslRequested,
    Denied,
}

impl AsyncPostgresShim {
    pub async fn run_on(socket: TcpStream, session: Arc<Session>) -> Result<(), Error> {
        let mut shim = Self {
            socket,
            parameters: HashMap::new(),
            portals: HashMap::new(),
            statements: HashMap::new(),
            session,
        };
        match shim.run().await {
            Err(e) => {
                if e.kind() == ErrorKind::UnexpectedEof
                    && shim.session.state.auth_context().is_none()
                {
                    return Ok(());
                }
                Err(e)
            }
            _ => {
                shim.socket.shutdown().await?;
                return Ok(());
            }
        }
    }

    pub async fn run(&mut self) -> Result<(), Error> {
        match self.process_startup_message().await? {
            StartupState::Success => {}
            StartupState::SslRequested => {
                if self.process_startup_message().await? != StartupState::Success {
                    return Ok(());
                }
            }
            StartupState::Denied => return Ok(()),
        }

        match buffer::read_message(&mut self.socket).await? {
            FrontendMessage::PasswordMessage(password_message) => {
                if !self.authenticate(password_message).await? {
                    return Ok(());
                }
            }
            _ => return Ok(()),
        }
        self.ready().await?;

        loop {
            match buffer::read_message(&mut self.socket).await? {
                FrontendMessage::Query(body) => self.process_query(body).await?,
                FrontendMessage::Parse(body) => self.parse(body).await?,
                FrontendMessage::Bind(body) => self.bind(body).await?,
                FrontendMessage::Execute(body) => self.execute(body).await?,
                FrontendMessage::Describe(body) => self.describe(body).await?,
                FrontendMessage::Sync => self.sync().await?,
                FrontendMessage::Terminate => return Ok(()),
                command_id => {
                    return Err(Error::new(
                        ErrorKind::Unsupported,
                        format!("Unsupported operation: {:?}", command_id),
                    ))
                }
            }
        }
    }

    pub async fn write<Message: protocol::Serialize>(
        &mut self,
        message: Message,
    ) -> Result<(), Error> {
        buffer::write_message(&mut self.socket, message).await
    }

    pub async fn process_startup_message(&mut self) -> Result<StartupState, Error> {
        let mut buffer = buffer::read_contents(&mut self.socket, 0).await?;

        let startup_message = protocol::StartupMessage::from(&mut buffer).await?;

        if startup_message.protocol_version.major == SSL_REQUEST_PROTOCOL {
            self.write(protocol::SSLResponse::new()).await?;
            return Ok(StartupState::SslRequested);
        }

        if startup_message.protocol_version.major != 3
            || startup_message.protocol_version.minor != 0
        {
            let error_response = protocol::ErrorResponse::new(
                protocol::ErrorSeverity::Fatal,
                protocol::ErrorCode::FeatureNotSupported,
                format!(
                    "unsupported frontend protocol {}.{}: server supports 3.0 to 3.0",
                    startup_message.protocol_version.major, startup_message.protocol_version.minor,
                ),
            );
            buffer::write_message(&mut self.socket, error_response).await?;
            return Ok(StartupState::Denied);
        }

        self.parameters = startup_message.parameters;
        if !self.parameters.contains_key("user") {
            let error_response = protocol::ErrorResponse::new(
                protocol::ErrorSeverity::Fatal,
                protocol::ErrorCode::InvalidAuthorizationSpecification,
                "no PostgreSQL user name specified in startup packet".to_string(),
            );
            buffer::write_message(&mut self.socket, error_response).await?;
            return Ok(StartupState::Denied);
        }
        if !self.parameters.contains_key("database") {
            self.parameters.insert(
                "database".to_string(),
                self.parameters.get("user").unwrap().clone(),
            );
        }

        self.write(protocol::Authentication::new(
            protocol::AuthenticationRequest::CleartextPassword,
        ))
        .await?;

        return Ok(StartupState::Success);
    }

    pub async fn authenticate(
        &mut self,
        password_message: protocol::PasswordMessage,
    ) -> Result<bool, Error> {
        let user = self.parameters.get("user").unwrap().clone();
        let authenticate_response = self
            .session
            .server
            .auth
            .authenticate(Some(user.clone()))
            .await;
        let mut auth_context: Option<AuthContext> = None;
        let auth_success = match authenticate_response {
            Ok(authenticate_response) => {
                auth_context = Some(authenticate_response.context);
                match authenticate_response.password {
                    None => true,
                    Some(password) => password == password_message.password,
                }
            }
            _ => false,
        };

        if !auth_success {
            let error_response = protocol::ErrorResponse::new(
                protocol::ErrorSeverity::Fatal,
                protocol::ErrorCode::InvalidPassword,
                format!("password authentication failed for user \"{}\"", &user),
            );
            buffer::write_message(&mut self.socket, error_response).await?;
            return Ok(false);
        }

        self.session.state.set_user(Some(user));
        self.session.state.set_auth_context(auth_context);

        self.write(protocol::Authentication::new(
            protocol::AuthenticationRequest::Ok,
        ))
        .await?;

        Ok(true)
    }

    pub async fn ready(&mut self) -> Result<(), Error> {
        let params = [
            ("server_version".to_string(), "14.2 (Cube SQL)".to_string()),
            ("server_encoding".to_string(), "UTF8".to_string()),
            ("client_encoding".to_string(), "UTF8".to_string()),
            ("DateStyle".to_string(), "ISO".to_string()),
        ];

        for (key, value) in params {
            self.write(protocol::ParameterStatus::new(key, value))
                .await?;
        }

        self.write(protocol::ReadyForQuery::new(
            protocol::TransactionStatus::Idle,
        ))
        .await?;

        Ok(())
    }

    pub async fn sync(&mut self) -> Result<(), Error> {
        self.write(protocol::ReadyForQuery::new(
            protocol::TransactionStatus::Idle,
        ))
        .await?;

        Ok(())
    }

    pub async fn describe(&mut self, describe: protocol::Describe) -> Result<(), Error> {
        Ok(())
    }

    pub async fn execute(&mut self, execute: protocol::Execute) -> Result<(), Error> {
        let portal = self.portals.get_mut(&execute.portal);
        match portal {
            Some(portal) => {
                portal.fetch();

                panic!("Unable to execute portal");
            }
            None => {
                self.write(protocol::ReadyForQuery::new(
                    protocol::TransactionStatus::Idle,
                ))
                .await?;
            }
        }

        Ok(())
    }

    pub async fn bind(&mut self, bind: protocol::Bind) -> Result<(), Error> {
        let mut source_statement = self
            .statements
            .get(&bind.statement)
            .ok_or_else(|| Error::new(ErrorKind::Other, "Unknown statement"))?;

        let prepated_statement = source_statement.bind(vec![]);

        let meta = self
            .session
            .server
            .transport
            .meta(self.auth_context().unwrap())
            .await
            .unwrap();

        let plan = convert_statement_to_cube_query(&prepated_statement, meta, self.session.clone())
            .unwrap();

        let portal = Portal { plan };

        self.portals.insert(bind.portal, portal);

        self.write(protocol::BindComplete::new()).await?;

        Ok(())
    }

    pub async fn parse(&mut self, parse: protocol::Parse) -> Result<(), Error> {
        let query = parse_sql_to_statement(&parse.query, DatabaseProtocol::PostgreSQL).unwrap();

        self.statements
            .insert(parse.name, PreparedStatement { query });

        self.write(protocol::ParseComplete::new()).await?;

        Ok(())
    }

    pub async fn process_query(&mut self, query: protocol::Query) -> Result<(), Error> {
        let query = query.query;
        debug!("Query: {}", query);
        match self.execute_query(&query).await {
            Err(e) => {
                let error_message = e.to_string();
                error!("Error during processing {}: {}", query, error_message);
                self.write(protocol::ErrorResponse::new(
                    protocol::ErrorSeverity::Error,
                    protocol::ErrorCode::InternalError,
                    error_message,
                ))
                .await?;
            }
            Ok(QueryResponse::Ok(_)) => {
                self.write(protocol::CommandComplete::new(
                    protocol::CommandCompleteTag::Select,
                    0,
                ))
                .await?;
            }
            Ok(QueryResponse::ResultSet(_, frame)) => {
                let mut fields = Vec::new();
                for column in frame.get_columns().iter() {
                    fields.push(protocol::RowDescriptionField::new(
                        column.get_name(),
                        PgType::get_by_tid(PgTypeId::TEXT),
                    ))
                }

                self.write(protocol::RowDescription::new(fields)).await?;

                for row in frame.get_rows().iter() {
                    let mut values = Vec::new();
                    for value in row.values().iter() {
                        let value = match value {
                            TableValue::Null => None,
                            TableValue::String(v) => Some(v.clone()),
                            TableValue::Int64(v) => Some(format!("{}", v)),
                            TableValue::Boolean(v) => {
                                Some((if *v { "t" } else { "v" }).to_string())
                            }
                            TableValue::Float64(v) => Some(format!("{}", v)),
                            TableValue::Timestamp(v) => Some(v.to_string()),
                        };
                        values.push(value);
                    }

                    self.write(protocol::DataRow::new(values)).await?;
                }

                self.write(protocol::CommandComplete::new(
                    protocol::CommandCompleteTag::Select,
                    0,
                ))
                .await?;
            }
        }

        self.write(protocol::ReadyForQuery::new(
            protocol::TransactionStatus::Idle,
        ))
        .await?;

        Ok(())
    }

    pub async fn execute_query(&mut self, query: &str) -> Result<QueryResponse, CubeError> {
        let meta = self
            .session
            .server
            .transport
            .meta(self.auth_context()?)
            .await?;

        let plan = convert_sql_to_cube_query(&query.to_string(), meta, self.session.clone())?;
        match plan {
            crate::compile::QueryPlan::MetaOk(status) => {
                return Ok(QueryResponse::Ok(status));
            }
            crate::compile::QueryPlan::MetaTabular(status, data_frame) => {
                return Ok(QueryResponse::ResultSet(status, data_frame));
            }
            crate::compile::QueryPlan::DataFusionSelect(status, plan, ctx) => {
                let df = DataFrameImpl::new(ctx.state, &plan);
                let batches = df.collect().await?;
                let response = batch_to_dataframe(&batches)?;

                return Ok(QueryResponse::ResultSet(status, Arc::new(response)));
            }
        }
    }

    pub(crate) fn auth_context(&self) -> Result<Arc<AuthContext>, CubeError> {
        if let Some(ctx) = self.session.state.auth_context() {
            Ok(Arc::new(ctx))
        } else {
            Err(CubeError::internal("must be auth".to_string()))
        }
    }
}

impl Drop for AsyncPostgresShim {
    fn drop(&mut self) {
        trace!(
            "[pg] Droping connection {}",
            self.session.state.connection_id
        );

        self.session
            .session_manager
            .drop_session(self.session.state.connection_id)
    }
}
