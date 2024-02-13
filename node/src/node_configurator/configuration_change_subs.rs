// Copyright (c) 2019-2024, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use crate::sub_lib::neighborhood::ConfigurationChangeMessage;
use actix::Recipient;

pub trait ConfigurationChangeSubs {
    fn recipients(&self) -> Vec<&Recipient<ConfigurationChangeMessage>>;

    fn send_msg_to_subs(&self, msg: ConfigurationChangeMessage) {
        self.recipients().iter().for_each(|recipient| {
            recipient
                .try_send(msg.clone())
                .expect("ConfigurationChangeMessage recipient is dead")
        })
    }
}

pub struct UpdateMinHopsSubs {
    pub neighborhood: Recipient<ConfigurationChangeMessage>,
}

impl ConfigurationChangeSubs for UpdateMinHopsSubs {
    fn recipients(&self) -> Vec<&Recipient<ConfigurationChangeMessage>> {
        vec![&self.neighborhood]
    }
}

pub struct UpdatePasswordSubs {
    pub neighborhood: Recipient<ConfigurationChangeMessage>,
}

impl ConfigurationChangeSubs for UpdatePasswordSubs {
    fn recipients(&self) -> Vec<&Recipient<ConfigurationChangeMessage>> {
        vec![&self.neighborhood]
    }
}

pub struct UpdateWalletsSubs {
    pub accountant: Recipient<ConfigurationChangeMessage>,
    pub blockchain_bridge: Recipient<ConfigurationChangeMessage>,
    pub neighborhood: Recipient<ConfigurationChangeMessage>,
}

impl ConfigurationChangeSubs for UpdateWalletsSubs {
    fn recipients(&self) -> Vec<&Recipient<ConfigurationChangeMessage>> {
        vec![
            &self.accountant,
            &self.blockchain_bridge,
            &self.neighborhood,
        ]
    }
}
