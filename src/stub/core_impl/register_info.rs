use super::prelude::*;
use crate::protocol::commands::ext::RegisterInfo;

impl<T: Target, C: Connection> GdbStubImpl<T, C> {
    pub(crate) fn handle_register_info(
        &mut self,
        res: &mut ResponseWriter<'_, C>,
        target: &mut T,
        command: RegisterInfo,
    ) -> Result<HandlerStatus, Error<T::Error, C::Error>> {
        let ops = match target.support_register_info() {
            Some(ops) => ops,
            None => return Ok(HandlerStatus::Handled),
        };

        crate::__dead_code_marker!("register_info", "impl");

        let handler_status = match command {
            RegisterInfo::qRegisterInfo(cmd) => {
                match ops.get_register_info(cmd.0) {
                    Some(info) => {
                        res.write_str(info)?;
                        HandlerStatus::Handled
                    },
                    None => HandlerStatus::NeedsOk
                }
            }
        };

        Ok(handler_status)
    }
}
