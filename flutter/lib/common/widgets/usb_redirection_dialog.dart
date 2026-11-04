import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/models/platform_model.dart';

/// USB redirection state per session, as pushed by the Rust side.
final Map<String, ValueNotifier<Map<String, dynamic>>> _usbStates = {};

ValueNotifier<Map<String, dynamic>> _usbState(SessionID sessionId) =>
    _usbStates.putIfAbsent(sessionId.toString(), () => ValueNotifier({}));

void onUsbRedirectionEvent(SessionID sessionId, String data) {
  try {
    _usbState(sessionId).value = jsonDecode(data) as Map<String, dynamic>;
  } catch (e) {
    debugPrint('usb_redirection: bad event $e');
  }
}

void _usbAction(SessionID sessionId, String action, [String busId = '']) {
  bind.sessionSetCommon(
      sessionId: sessionId,
      key: 'usb-redirection',
      value: jsonEncode({'action': action, 'bus_id': busId}));
}

/// Lists the USB devices of this computer and shares one with the peer.
void showUsbRedirectionDialog(
    SessionID sessionId, OverlayDialogManager dialogManager) {
  _usbAction(sessionId, 'list');
  final state = _usbState(sessionId);
  dialogManager.show((setState, close, context) {
    return CustomAlertDialog(
      title: Text(translate('USB devices')),
      contentBoxConstraints: const BoxConstraints(minWidth: 480, maxWidth: 600),
      content: ValueListenableBuilder<Map<String, dynamic>>(
        valueListenable: state,
        builder: (context, value, _) {
          final devices = (value['devices'] as List?) ?? [];
          final error = (value['error'] as String?) ?? '';
          return Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(translate('usb-redirection-tip'),
                  style: Theme.of(context).textTheme.bodySmall),
              const SizedBox(height: 8),
              if (devices.isEmpty) Text(translate('No USB devices')),
              ...devices.map((d) => _deviceTile(sessionId, d)),
              if (error.isNotEmpty)
                Padding(
                  padding: const EdgeInsets.only(top: 8),
                  child: Text(error,
                      style: TextStyle(color: Theme.of(context).colorScheme.error)),
                ),
            ],
          );
        },
      ),
      actions: [
        dialogButton('Refresh',
            onPressed: () => _usbAction(sessionId, 'list'), isOutline: true),
        dialogButton('Close', onPressed: close),
      ],
      onCancel: close,
    );
  }, clickMaskDismiss: true, backDismiss: true);
}

Widget _deviceTile(SessionID sessionId, dynamic d) {
  final busId = d['bus_id'] as String? ?? '';
  final state = d['state'] as String? ?? 'idle';
  final description = d['description'] as String? ?? '';
  final ids = '${d['vid']}:${d['pid']}';
  Widget trailing;
  switch (state) {
    case 'attached':
    case 'sharing':
      trailing = dialogButton('Stop',
          onPressed: () => _usbAction(sessionId, 'stop', busId),
          isOutline: true);
      break;
    case 'stopping':
      trailing = Text(translate('Stopping...'));
      break;
    default:
      trailing = dialogButton('Share',
          onPressed: () => _usbAction(sessionId, 'share', busId));
  }
  return ListTile(
    dense: true,
    contentPadding: EdgeInsets.zero,
    leading: Icon(state == 'attached' ? Icons.usb : Icons.usb_off),
    title: Text(description.isEmpty ? ids : description),
    subtitle: Text('$busId  $ids  ${translate(state)}'),
    trailing: trailing,
  );
}
