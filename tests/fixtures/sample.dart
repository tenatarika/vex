import 'dart:async';

abstract class Repository {
  void save();
}

mixin Logging {
  void log() {}
}

class Service extends Repository with Logging {
  Service();
  @override
  void save() {
    helper();
    this.log();
  }
  void refresh() => helper();
}

void helper() {}

void run() {
  final service = Service();
  service.save();
  service..save()..refresh();
}
