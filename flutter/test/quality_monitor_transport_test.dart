import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/models/model.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:uuid/uuid.dart';

final _sessionId = UuidValue('00000000-0000-0000-0000-000000000000');

class _FakeFFI implements FFI {
  @override
  UuidValue get sessionId => _sessionId;

  @override
  late final FfiModel ffiModel = FfiModel(WeakReference(this));

  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

void main() {
  test('GDesk metrics survive delay-only events and clear when the codec changes', () {
    final ffi = _FakeFFI();
    final model = QualityMonitorModel(WeakReference(ffi));
    model.updateQualityStatus({
      'codec_format': 'GDesk',
      'gdesk_photon_latency': '12.50 ms',
      'gdesk_codec_us': '1200/300 us',
      'gdesk_p99_jitter': '1.00 ms',
      'gdesk_cache_hit_rate': '75.0%',
      'gdesk_qos_light': 'Yellow',
      'gdesk_realtime_bandwidth': '800 kbps',
      'gdesk_active_backend': 'GpuVulkan',
    });
    expect(model.data.gdeskPhotonLatency, '12.50 ms');
    expect(model.data.gdeskCodecUs, '1200/300 us');
    expect(model.data.gdeskP99Jitter, '1.00 ms');
    expect(model.data.gdeskCacheHitRate, '75.0%');
    expect(model.data.gdeskQosLight, 'Yellow');
    expect(model.data.gdeskRealtimeBandwidth, '800 kbps');
    expect(model.data.gdeskActiveBackend, 'GpuVulkan');
    model.updateQualityStatus({'delay': '20', 'gdesk_qos_light': ''});
    expect(model.data.gdeskQosLight, 'Yellow');
    expect(model.data.gdeskActiveBackend, 'GpuVulkan');
    model.updateQualityStatus({'codec_format': 'VP9'});
    expect(model.data.gdeskPhotonLatency, isNull);
    expect(model.data.gdeskQosLight, isNull);
    expect(model.data.gdeskRealtimeBandwidth, isNull);
    expect(model.data.gdeskActiveBackend, isNull);
  });

  test('the quality monitor names the WebRTC transport only on web', () {
    final ffi = _FakeFFI();
    ffi.ffiModel.cachedPeerData.streamType = 'WebRTC';
    final model = QualityMonitorModel(WeakReference(ffi));
    // Off the web the session tab's tooltip already names the transport.
    expect(isWeb, isFalse);
    expect(model.webrtcTransport, isNull);
  });
}
