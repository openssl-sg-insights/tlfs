mod secrets;
mod sync;

pub use libp2p::Multiaddr;
pub use tlfs_crdt::{Causal, DocId, Hash, Kind, Lens, PeerId, Permission, PrimitiveKind};

use crate::secrets::{Metadata, Secrets};
use crate::sync::{Behaviour, ToLibp2pKeypair, ToLibp2pPublic};
use anyhow::Result;
use futures::channel::{mpsc, oneshot};
use futures::future::poll_fn;
use futures::stream::Stream;
use libp2p::Swarm;
use std::pin::Pin;
use std::task::Poll;
use tlfs_crdt::{Backend, Doc, Frontend};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::EnvFilter;

pub struct Sdk {
    frontend: Frontend,
    secrets: Secrets,
    swarm: mpsc::UnboundedSender<Command>,
}

impl Sdk {
    pub async fn new(db: sled::Db) -> Result<Self> {
        tracing_log::LogTracer::init().ok();
        let env = std::env::var(EnvFilter::DEFAULT_ENV).unwrap_or_else(|_| "info".to_owned());
        let subscriber = tracing_subscriber::FmtSubscriber::builder()
            .with_span_events(FmtSpan::ACTIVE | FmtSpan::CLOSE)
            .with_env_filter(EnvFilter::new(env))
            .with_writer(std::io::stderr)
            .finish();
        tracing::subscriber::set_global_default(subscriber).ok();
        log_panics::init();

        let backend = Backend::new(db.clone())?;
        let frontend = backend.frontend();
        let secrets = Secrets::new(db.open_tree("secrets")?);

        if secrets.keypair(Metadata::new())?.is_none() {
            secrets.generate_keypair(Metadata::new())?;
        }
        let keypair = secrets.keypair(Metadata::new())?.unwrap();

        let transport = libp2p::development_transport(keypair.to_libp2p()).await?;
        let behaviour = Behaviour::new(backend, secrets.clone())?;
        let mut swarm = Swarm::new(
            transport,
            behaviour,
            keypair.peer_id().to_libp2p().into_peer_id(),
        );
        swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse().unwrap())?;

        let (tx, mut rx) = mpsc::unbounded();
        async_global_executor::spawn::<_, ()>(poll_fn(move |cx| {
            while let Poll::Ready(Some(cmd)) = Pin::new(&mut rx).poll_next(cx) {
                match cmd {
                    Command::AddAddress(peer, addr) => {
                        swarm.behaviour_mut().add_address(&peer, addr)
                    }
                    Command::RemoveAddress(peer, addr) => {
                        swarm.behaviour_mut().remove_address(&peer, &addr)
                    }
                    Command::Addresses(ch) => {
                        let addrs = swarm.listeners().cloned().collect::<Vec<_>>();
                        ch.send(addrs).ok();
                    }
                    Command::Publish(causal) => {
                        swarm.behaviour_mut().send_delta(&causal).ok();
                    }
                    Command::Subscribe(id) => {
                        swarm.behaviour_mut().subscribe_doc(&id).ok();
                    }
                };
            }
            while let Poll::Ready(_) = swarm.behaviour_mut().poll_backend(cx) {}
            while let Poll::Ready(_) = Pin::new(&mut swarm).poll_next(cx) {}
            Poll::Pending
        }))
        .detach();

        Ok(Self {
            frontend,
            secrets,
            swarm: tx,
        })
    }

    pub async fn memory() -> Result<Self> {
        Self::new(sled::Config::new().temporary(true).open()?).await
    }

    pub fn peer_id(&self) -> Result<PeerId> {
        Ok(self.secrets.keypair(Metadata::new())?.unwrap().peer_id())
    }

    pub fn add_address(&self, peer: PeerId, addr: Multiaddr) {
        self.swarm
            .unbounded_send(Command::AddAddress(peer, addr))
            .ok();
    }

    pub fn remove_address(&self, peer: PeerId, addr: Multiaddr) {
        self.swarm
            .unbounded_send(Command::RemoveAddress(peer, addr))
            .ok();
    }

    pub async fn addresses(&self) -> Vec<Multiaddr> {
        let (tx, rx) = oneshot::channel();
        if let Ok(()) = self.swarm.unbounded_send(Command::Addresses(tx)) {
            if let Ok(addrs) = rx.await {
                return addrs;
            }
        }
        vec![]
    }

    pub fn register(&self, lenses: Vec<Lens>) -> Result<Hash> {
        self.frontend.register(lenses)
    }

    pub fn docs(&self) -> impl Iterator<Item = Result<DocId>> + '_ {
        self.frontend.docs()
    }

    pub fn create_doc(&self) -> Result<Doc> {
        let peer_id = self.peer_id()?;
        let doc = self.frontend.create_doc(peer_id)?;
        self.swarm
            .unbounded_send(Command::Subscribe(*doc.id()))
            .ok();
        Ok(doc)
    }

    pub fn add_doc(&self, id: DocId) -> Result<Doc> {
        let peer_id = self.peer_id()?;
        let doc = self.frontend.add_doc(id, peer_id)?;
        self.swarm
            .unbounded_send(Command::Subscribe(*doc.id()))
            .ok();
        Ok(doc)
    }

    pub fn doc(&self, id: DocId) -> Result<Doc> {
        let doc = self.frontend.doc(id)?;
        self.swarm
            .unbounded_send(Command::Subscribe(*doc.id()))
            .ok();
        Ok(doc)
    }

    pub fn remove_doc(&self, id: DocId) -> Result<()> {
        self.frontend.remove_doc(id)
    }

    pub fn apply(&self, causal: Causal) -> Result<()> {
        self.frontend.apply(&causal)?;
        self.swarm.unbounded_send(Command::Publish(causal)).ok();
        Ok(())
    }
}

enum Command {
    AddAddress(PeerId, Multiaddr),
    RemoveAddress(PeerId, Multiaddr),
    Addresses(oneshot::Sender<Vec<Multiaddr>>),
    Publish(Causal),
    Subscribe(DocId),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[async_std::test]
    #[ignore]
    async fn test_api() -> Result<()> {
        let sdk = Sdk::memory().await?;
        let doc = sdk.create_doc()?;
        assert!(doc.cursor().can(&sdk.peer_id()?, Permission::Write)?);

        let lenses = vec![
            Lens::Make(Kind::Struct),
            Lens::AddProperty("todos".into()),
            Lens::Make(Kind::Table(PrimitiveKind::U64)).lens_in("todos"),
            Lens::Make(Kind::Struct).lens_map_value().lens_in("todos"),
            Lens::AddProperty("title".into())
                .lens_map_value()
                .lens_in("todos"),
            Lens::Make(Kind::Reg(PrimitiveKind::Str))
                .lens_in("title")
                .lens_map_value()
                .lens_in("todos"),
            Lens::AddProperty("complete".into())
                .lens_map_value()
                .lens_in("todos"),
            Lens::Make(Kind::Flag)
                .lens_in("complete")
                .lens_map_value()
                .lens_in("todos"),
        ];
        let hash = sdk.register(lenses)?;
        sdk.transform(doc.id(), hash)?;

        let title = "something that needs to be done";
        let delta = doc
            .cursor()
            .field("todos")?
            .key(&0u64.into())?
            .field("title")?
            .assign(title)?;
        sdk.apply(delta)?;

        let value = doc
            .cursor()
            .field("todos")?
            .key(&0u64.into())?
            .field("title")?
            .strs()?
            .next()
            .unwrap()?;
        assert_eq!(value, title);

        let sdk2 = Sdk::memory().await?;
        let op = doc
            .cursor()
            .say_can(Some(sdk2.peer_id()?), Permission::Write)?;
        sdk.apply(op)?;

        for addr in sdk.addresses() {
            sdk2.add_address(sdk.peer_id()?, addr);
        }
        let doc2 = sdk2.add_doc(*doc.id())?;
        // TODO: wait for unjoin

        let value = doc2
            .cursor()
            .field("todos")?
            .key(&0u64.into())?
            .field("title")?
            .strs()?
            .next()
            .unwrap()?;
        assert_eq!(value, title);

        Ok(())
    }
}
