/// The page, as Dart code in this browser sees it.
///
/// Every call here goes through the browser's host dispatcher (browser/src/host.rs) and shows
/// up in its Trace panel. Elements are small integer handles; the host owns the real nodes.
library;

import 'dart:js_interop';

@JS('host.domQuery')
external int _query(String selector);

@JS('host.domCreate')
external int _create(String tag);

@JS('host.domAppend')
external void _append(int parent, int child);

@JS('host.domSetText')
external void _setText(int node, String text);

@JS('host.domOnClick')
external void _onClick(int node, JSFunction callback);

/// A node in the page.
class Element {
  final int _handle;
  Element._(this._handle);

  /// Replaces the element's children with [value] as text.
  set text(String value) => _setText(_handle, value);

  void append(Element child) => _append(_handle, child._handle);

  void onClick(void Function() handler) => _onClick(_handle, handler.toJS);
}

/// The first element matching [selector], or null.
Element? query(String selector) {
  final handle = _query(selector);
  return handle < 0 ? null : Element._(handle);
}

/// A new, unattached element.
Element create(String tag) => Element._(_create(tag));
