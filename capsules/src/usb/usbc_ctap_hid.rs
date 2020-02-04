//! A USB HID client of the USB hardware interface

use super::descriptors::Buffer64;
use super::descriptors::ConfigurationDescriptor;
use super::descriptors::DescriptorType;
use super::descriptors::DeviceDescriptor;
use super::descriptors::EndpointAddress;
use super::descriptors::EndpointDescriptor;
use super::descriptors::HIDCountryCode;
use super::descriptors::HIDDescriptor;
use super::descriptors::HIDSubordinateDescriptor;
use super::descriptors::InterfaceDescriptor;
use super::descriptors::ReportDescriptor;
use super::descriptors::TransferDirection;
use super::usb_ctap::CtapUsbClient;
use super::usbc_client_ctrl::ClientCtrl;
use core::cell::Cell;
use kernel::common::cells::OptionalCell;
use kernel::debug;
use kernel::hil;
use kernel::hil::usb::TransferType;

const VENDOR_ID: u16 = 0x1915; // Nordic Semiconductor
const PRODUCT_ID: u16 = 0x521f; // nRF52840 Dongle (PCA10059)

static LANGUAGES: &'static [u16; 1] = &[
    0x0409, // English (United States)
];

static STRINGS: &'static [&'static str] = &[
    // Manufacturer
    "Nordic Semiconductor ASA",
    // Product
    "OpenSK",
    // Serial number
    "v0.1",
];

static ENDPOINTS: &'static [EndpointDescriptor] = &[
    EndpointDescriptor {
        endpoint_address: EndpointAddress::new_const(1, TransferDirection::HostToDevice),
        transfer_type: TransferType::Interrupt,
        max_packet_size: 64,
        interval: 5,
    },
    EndpointDescriptor {
        endpoint_address: EndpointAddress::new_const(1, TransferDirection::DeviceToHost),
        transfer_type: TransferType::Interrupt,
        max_packet_size: 64,
        interval: 5,
    },
];

static CTAP_REPORT_DESCRIPTOR: &'static [u8] = &[
    0x06, 0xD0, 0xF1, // HID_UsagePage ( FIDO_USAGE_PAGE ),
    0x09, 0x01, // HID_Usage ( FIDO_USAGE_CTAPHID ),
    0xA1, 0x01, // HID_Collection ( HID_Application ),
    0x09, 0x20, // HID_Usage ( FIDO_USAGE_DATA_IN ),
    0x15, 0x00, // HID_LogicalMin ( 0 ),
    0x26, 0xFF, 0x00, // HID_LogicalMaxS ( 0xff ),
    0x75, 0x08, // HID_ReportSize ( 8 ),
    0x95, 0x40, // HID_ReportCount ( HID_INPUT_REPORT_BYTES ),
    0x81, 0x02, // HID_Input ( HID_Data | HID_Absolute | HID_Variable ),
    0x09, 0x21, // HID_Usage ( FIDO_USAGE_DATA_OUT ),
    0x15, 0x00, // HID_LogicalMin ( 0 ),
    0x26, 0xFF, 0x00, // HID_LogicalMaxS ( 0xff ),
    0x75, 0x08, // HID_ReportSize ( 8 ),
    0x95, 0x40, // HID_ReportCount ( HID_OUTPUT_REPORT_BYTES ),
    0x91, 0x02, // HID_Output ( HID_Data | HID_Absolute | HID_Variable ),
    0xC0, // HID_EndCollection
];

static CTAP_REPORT: ReportDescriptor<'static> = ReportDescriptor {
    desc: CTAP_REPORT_DESCRIPTOR,
};

static HID_SUB_DESCRIPTORS: &'static [HIDSubordinateDescriptor] = &[HIDSubordinateDescriptor {
    typ: DescriptorType::Report,
    len: CTAP_REPORT_DESCRIPTOR.len() as u16,
}];

static HID: HIDDescriptor<'static> = HIDDescriptor {
    hid_class: 0x0110,
    country_code: HIDCountryCode::NotSupported,
    sub_descriptors: HID_SUB_DESCRIPTORS,
};

pub struct ClientCtapHID<'a, 'b, C: 'a> {
    client_ctrl: ClientCtrl<'a, 'static, C>,

    // A 64-byte buffer for the endpoint
    buffer: Buffer64,

    // Interaction with the client
    client: OptionalCell<&'b dyn CtapUsbClient>,
    tx_packet: OptionalCell<[u8; 64]>,
    pending_in: Cell<bool>,
    pending_out: Cell<bool>,
    delayed_out: Cell<bool>,
}

impl<'a, 'b, C: hil::usb::UsbController<'a>> ClientCtapHID<'a, 'b, C> {
    pub fn new(controller: &'a C) -> Self {
        ClientCtapHID {
            client_ctrl: ClientCtrl::new(
                controller,
                DeviceDescriptor {
                    // TODO: set this field at the board level.
                    max_packet_size_ep0: 64,
                    vendor_id: VENDOR_ID,
                    product_id: PRODUCT_ID,
                    manufacturer_string: 1,
                    product_string: 2,
                    serial_number_string: 3,
                    ..Default::default()
                },
                ConfigurationDescriptor {
                    // Must be non-zero, otherwise dmesg prints the following error:
                    // [...] usb 2-3: config 0 descriptor??
                    configuration_value: 1,
                    ..Default::default()
                },
                // Interface declared in the FIDO2 specification, section 8.1.8.1
                InterfaceDescriptor {
                    interface_class: 0x03, // HID
                    interface_subclass: 0x00,
                    interface_protocol: 0x00,
                    ..Default::default()
                },
                ENDPOINTS,
                Some(&HID),
                Some(&CTAP_REPORT),
                LANGUAGES,
                STRINGS,
            ),
            buffer: Default::default(),
            client: OptionalCell::empty(),
            tx_packet: OptionalCell::empty(),
            pending_in: Cell::new(false),
            pending_out: Cell::new(false),
            delayed_out: Cell::new(false),
        }
    }

    pub fn set_client(&'a self, client: &'b dyn CtapUsbClient) {
        self.client.set(client);
    }

    pub fn transmit_packet(&'a self, packet: &[u8]) -> bool {
        if self.pending_in.get() {
            // The previous packet has not yet been transmitted, reject the new one.
            false
        } else {
            self.pending_in.set(true);
            let mut buf: [u8; 64] = [0; 64];
            buf.copy_from_slice(packet);
            self.tx_packet.set(buf);
            // Alert the controller that we now have data to send on the Interrupt IN endpoint.
            self.controller().endpoint_resume_in(1);
            true
        }
    }

    pub fn receive_packet(&'a self) -> bool {
        if self.pending_out.get() {
            // The previous packet has not yet been received, reject the new one.
            false
        } else {
            self.pending_out.set(true);
            // In case we reported Delay before, send the pending packet back to the client.
            // Otherwise, there's nothing to do, the controller will send us a packet_out when a
            // packet arrives.
            if self.delayed_out.take() {
                if self.send_packet_to_client() {
                    // If that succeeds, alert the controller that we can now
                    // receive data on the Interrupt OUT endpoint.
                    self.controller().endpoint_resume_out(1);
                }
            }
            true
        }
    }

    // Send an OUT packet available in the controller back to the client.
    // This returns false if the client is not ready to receive a packet, and true if the client
    // successfully accepted the packet.
    fn send_packet_to_client(&'a self) -> bool {
        // Copy the packet into a buffer to send to the client.
        let mut buf: [u8; 64] = [0; 64];
        for (i, x) in self.buffer.buf.iter().enumerate() {
            buf[i] = x.get();
        }

        assert!(!self.delayed_out.get());

        // Notify the client
        if self
            .client
            .map_or(false, |client| client.can_receive_packet())
        {
            assert!(self.pending_out.take());

            // Clear any pending packet on the transmitting side.
            // It's up to the client to handle the received packet and decide if this packet
            // should be re-transmitted or not.
            self.cancel_in_transaction();

            self.client.map(|client| client.packet_received(&buf));
            true
        } else {
            // Cannot receive now, indicate a delay to the controller.
            self.delayed_out.set(true);
            false
        }
    }

    pub fn cancel_transaction(&'a self) -> bool {
        self.cancel_in_transaction() | self.cancel_out_transaction()
    }

    fn cancel_in_transaction(&'a self) -> bool {
        self.tx_packet.take();
        let result = self.pending_in.take();
        if result {
            self.controller().endpoint_cancel_in(1);
        }
        result
    }

    fn cancel_out_transaction(&'a self) -> bool {
        self.pending_out.take()
    }

    #[inline]
    fn controller(&'a self) -> &'a C {
        self.client_ctrl.controller()
    }
}

impl<'a, 'b, C: hil::usb::UsbController<'a>> hil::usb::Client<'a> for ClientCtapHID<'a, 'b, C> {
    fn enable(&'a self) {
        // Set up the default control endpoint
        self.client_ctrl.enable();

        // Set up the interrupt in-out endpoint
        self.controller().endpoint_set_buffer(1, &self.buffer.buf);
        self.controller()
            .endpoint_in_out_enable(TransferType::Interrupt, 1);
    }

    fn attach(&'a self) {
        self.client_ctrl.attach();
    }

    fn bus_reset(&'a self) {
        // Should the client initiate reconfiguration here?
        // For now, the hardware layer does it.

        debug!("Bus reset");
    }

    /// Handle a Control Setup transaction
    fn ctrl_setup(&'a self, endpoint: usize) -> hil::usb::CtrlSetupResult {
        self.client_ctrl.ctrl_setup(endpoint)
    }

    /// Handle a Control In transaction
    fn ctrl_in(&'a self, endpoint: usize) -> hil::usb::CtrlInResult {
        self.client_ctrl.ctrl_in(endpoint)
    }

    /// Handle a Control Out transaction
    fn ctrl_out(&'a self, endpoint: usize, packet_bytes: u32) -> hil::usb::CtrlOutResult {
        self.client_ctrl.ctrl_out(endpoint, packet_bytes)
    }

    fn ctrl_status(&'a self, endpoint: usize) {
        self.client_ctrl.ctrl_status(endpoint)
    }

    /// Handle the completion of a Control transfer
    fn ctrl_status_complete(&'a self, endpoint: usize) {
        self.client_ctrl.ctrl_status_complete(endpoint)
    }

    /// Handle a Bulk/Interrupt IN transaction
    fn packet_in(&'a self, transfer_type: TransferType, endpoint: usize) -> hil::usb::InResult {
        match transfer_type {
            TransferType::Bulk => hil::usb::InResult::Error,
            TransferType::Interrupt => {
                if endpoint != 1 {
                    return hil::usb::InResult::Error;
                }

                if let Some(packet) = self.tx_packet.take() {
                    let buf = &self.buffer.buf;
                    for i in 0..64 {
                        buf[i].set(packet[i]);
                    }

                    hil::usb::InResult::Packet(64)
                } else {
                    // Nothing to send
                    hil::usb::InResult::Delay
                }
            }
            TransferType::Control | TransferType::Isochronous => unreachable!(),
        }
    }

    /// Handle a Bulk/Interrupt OUT transaction
    fn packet_out(
        &'a self,
        transfer_type: TransferType,
        endpoint: usize,
        packet_bytes: u32,
    ) -> hil::usb::OutResult {
        match transfer_type {
            TransferType::Bulk => hil::usb::OutResult::Error,
            TransferType::Interrupt => {
                if endpoint != 1 {
                    return hil::usb::OutResult::Error;
                }

                if packet_bytes != 64 {
                    // Cannot process this packet
                    hil::usb::OutResult::Error
                } else {
                    if self.send_packet_to_client() {
                        hil::usb::OutResult::Ok
                    } else {
                        hil::usb::OutResult::Delay
                    }
                }
            }
            TransferType::Control | TransferType::Isochronous => unreachable!(),
        }
    }

    fn packet_transmitted(&'a self, endpoint: usize) {
        if endpoint != 1 {
            panic!("Unexpected transmission on ep {}", endpoint);
        }

        if self.tx_packet.is_some() {
            panic!("Unexpected tx_packet while a packet was being transmitted.");
        }
        self.pending_in.set(false);
        // Notify the client
        self.client.map(|client| client.packet_transmitted());
    }
}
